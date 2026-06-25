use bytes::Bytes;
use restream::db;
use restream::media::codec::{audio_for_ts, video_for_ts};
use restream::media::engine::MediaEngine;
use restream::media::ring_buffer::{
    DtsEnforcer, MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer,
};
use restream::media::rtmp::{start_rtmp_egress, start_rtmp_server_on};
use restream::media::security::{DEFAULT_INGEST_SECURITY_CONFIG, IngestSecurityService};
use restream::media::srt::{SrtServer, start_srt_egress};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Barrier;

const RTMP_PORT: u16 = 11935;
const SRT_PORT: u16 = 11080;
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

async fn run() -> Result<(), String> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let result = match command.as_str() {
        "correctness" => correctness().await,
        "correctness-rtmp" => correctness_rtmp().await,
        "correctness-srt" => correctness_srt().await,
        "matrix" => matrix_correctness().await,
        "matrix-in-memory" => matrix_correctness_in_memory().await,
        "egress" => egress_correctness().await,
        "correctness-hevc-rtmp" => hevc_rtmp_egress_correctness().await,
        "correctness-hevc-srt" => hevc_srt_passthrough_correctness().await,
        "hevc-load" => hevc_load_test().await,
        "in-process" => in_process_load(500, 2_000).await,
        "network" => network_load(32, Duration::from_secs(5)).await,
        "all" => {
            let correctness = correctness().await?;
            let hevc_rtmp = hevc_rtmp_egress_correctness().await?;
            let in_process = in_process_load(500, 2_000).await?;
            let network = network_load(32, Duration::from_secs(5)).await?;
            // egress_correctness calls process::exit to avoid FFmpeg/SRT
            // teardown segfaults, so it must run last.
            let egress = egress_correctness().await?;
            Ok(json!({
                "correctness": correctness,
                "correctnessHevcRtmp": hevc_rtmp,
                "egress": egress,
                "inProcess": in_process,
                "network": network,
            }))
        }
        other => Err(format!(
            "unknown command {other:?}; use correctness, correctness-rtmp, correctness-srt, \
              matrix, matrix-in-memory, egress, correctness-hevc-rtmp, correctness-hevc-srt, \
              hevc-load, in-process, network, or all"
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
    Path::new("test/artifacts/latest").join(name)
}

async fn correctness() -> Result<Value, String> {
    let db_path = artifact_path("correctness.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;
    db::create_pipeline(&pool, "pipe-rtmp", "RTMP test", "e2e-rtmp", None, None)
        .await
        .map_err(|e| e.to_string())?;
    db::create_pipeline(&pool, "pipe-srt", "SRT test", "e2e-srt", None, None)
        .await
        .map_err(|e| e.to_string())?;

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let rtmp_task = tokio::spawn(start_rtmp_server_on(
        pool.clone(),
        security.clone(),
        engine.clone(),
        RTMP_PORT,
    ));
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone(), security));
    let srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let rtmp_fixture = artifact_path("correctness-h264.ts");
    generate_fixture_h264(&rtmp_fixture).await?;
    let srt_fixture = artifact_path("correctness-h265.ts");
    generate_fixture_h265(&srt_fixture).await?;

    let mut rtmp_publisher = spawn_publisher(
        &rtmp_fixture,
        &format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp"),
        "flv",
        false,
    )
    .await?;
    let mut srt_publisher = spawn_publisher(
        &srt_fixture,
        &format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt&pkt_size=1316"),
        "mpegts",
        true,
    )
    .await?;

    wait_for_ingests(&engine, &["pipe-rtmp", "pipe-srt"], Duration::from_secs(12)).await?;

    let rtmp_snapshot = engine
        .probe_snapshot("pipe-rtmp")
        .await
        .ok_or("missing RTMP snapshot")?;
    let srt_snapshot = engine
        .probe_snapshot("pipe-srt")
        .await
        .ok_or("missing SRT snapshot")?;

    let rtmp_probe = ffprobe(&format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp")).await?;
    let srt_probe = ffprobe(&format!(
        "srt://127.0.0.1:{SRT_PORT}?streamid=read:live/e2e-srt&mode=caller&transtype=live&latency=100"
    ))
    .await?;

    assert_media_only(&rtmp_probe, "RTMP read")?;
    assert_media_only(&srt_probe, "SRT read")?;
    let rtmp_media = normalized_streams(&rtmp_probe)?;
    let srt_media = normalized_streams(&srt_probe)?;

    assert_snapshot_matches_probe(&rtmp_snapshot, &rtmp_media, "RTMP")?;
    assert_snapshot_matches_probe(&srt_snapshot, &srt_media, "SRT")?;

    stop_child(&mut rtmp_publisher).await;
    stop_child(&mut srt_publisher).await;
    rtmp_task.abort();
    srt_task.abort();

    Ok(json!({
        "passed": true,
        "rtmp": {
            "fixture": rtmp_fixture,
            "publishUrl": format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp"),
            "readUrl": format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp"),
            "snapshot": rtmp_snapshot,
            "probe": rtmp_probe,
            "normalizedStreams": rtmp_media,
        },
        "srt": {
            "fixture": srt_fixture,
            "publishUrl": format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt"),
            "readUrl":         format!("srt://127.0.0.1:{SRT_PORT}?streamid=read:live/e2e-srt&mode=caller&transtype=live&latency=100"),
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

async fn correctness_one_protocol(protocol: &str) -> Result<Value, String> {
    let db_path = artifact_path(&format!("correctness-{protocol}.sqlite"));
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;
    let pipeline_id = format!("pipe-{protocol}");
    let stream_key = format!("e2e-{protocol}");
    db::create_pipeline(
        &pool,
        &pipeline_id,
        &format!("{protocol} test"),
        &stream_key,
        None,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let _server_task = if protocol == "rtmp" {
        tokio::spawn(start_rtmp_server_on(
            pool,
            security,
            engine.clone(),
            RTMP_PORT,
        ))
    } else {
        let srt_server = Arc::new(SrtServer::new(pool, engine.clone(), security));
        tokio::spawn(srt_server.run(SRT_PORT))
    };
    tokio::time::sleep(Duration::from_millis(500)).await;

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
            format!("rtmp://127.0.0.1:{RTMP_PORT}/live/{stream_key}"),
            format!("rtmp://127.0.0.1:{RTMP_PORT}/live/{stream_key}"),
            "flv",
        )
    } else {
        (
            format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/{stream_key}&pkt_size=1316"),
            format!(
                "srt://127.0.0.1:{SRT_PORT}?streamid=read:live/{stream_key}&mode=caller&transtype=live&latency=100"
            ),
            "mpegts",
        )
    };
    let map_all = protocol == "srt";
    let mut publisher = spawn_publisher(&fixture, &publish_url, format, map_all).await?;
    wait_for_ingests(&engine, &[&pipeline_id], Duration::from_secs(12)).await?;
    let snapshot = engine
        .probe_snapshot(&pipeline_id)
        .await
        .ok_or_else(|| format!("missing {protocol} snapshot"))?;
    let probe = ffprobe(&read_url).await?;
    assert_media_only(&probe, &format!("{protocol} read"))?;
    let normalized = normalized_streams(&probe)?;
    assert_snapshot_matches_probe(&snapshot, &normalized, protocol)?;
    stop_child(&mut publisher).await;

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
    let db_path = artifact_path("egress.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;

    for (id, name, key) in [
        ("pipe-src", "RTMP source", "e2e-src"),
        ("pipe-rtmp-sink", "RTMP egress sink", "e2e-rtmp-sink"),
        ("pipe-srt-sink", "SRT egress sink", "e2e-srt-sink"),
    ] {
        db::create_pipeline(&pool, id, name, key, None, None)
            .await
            .map_err(|e| e.to_string())?;
    }

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    // Keep the server tasks alive until the deliberate `_exit` at the end.
    let _rtmp_task = tokio::spawn(start_rtmp_server_on(
        pool.clone(),
        security.clone(),
        engine.clone(),
        RTMP_PORT,
    ));
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone(), security));
    let _srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture_h264(&fixture).await?;
    }

    // Retain the publisher process handle so it remains alive for the probes.
    let _publisher = spawn_publisher(
        &fixture,
        &format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-src"),
        "flv",
        false,
    )
    .await?;
    wait_for_ingests(&engine, &["pipe-src"], Duration::from_secs(12)).await?;

    let source_ring = engine.get_or_create_pipeline("pipe-src").await;

    let rtmp_egress_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp-sink");
    let rtmp_token = engine
        .register_egress("out-rtmp", "pipe-src", &rtmp_egress_url)
        .await;
    let _rtmp_egress = tokio::spawn(start_rtmp_egress(
        "out-rtmp".to_string(),
        "pipe-src".to_string(),
        rtmp_egress_url.clone(),
        source_ring.clone(),
        engine.clone(),
        rtmp_token.clone(),
    ));

    let srt_egress_url =
        format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt-sink&pkt_size=1316");
    let srt_token = engine
        .register_egress("out-srt", "pipe-src", &srt_egress_url)
        .await;
    let _srt_egress = tokio::spawn(start_srt_egress(
        "out-srt".to_string(),
        "pipe-src".to_string(),
        "source".to_string(),
        srt_egress_url.clone(),
        source_ring,
        engine.clone(),
        srt_token.clone(),
    ));

    let egress_up = wait_for_ingests(
        &engine,
        &["pipe-rtmp-sink", "pipe-srt-sink"],
        Duration::from_secs(15),
    )
    .await;

    let mut results = json!({});

    if let Err(ref e) = egress_up {
        results["rtmpEgress"] = json!({"passed": false, "error": e.to_string()});
        results["srtEgress"] = json!({"passed": false, "error": e.to_string()});
    } else {
        tokio::time::sleep(Duration::from_secs(3)).await;

        let rtmp_read_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp-sink");
        let rtmp_result = ffprobe(&rtmp_read_url).await;
        let rtmp_validation = rtmp_result
            .as_ref()
            .map_err(|error| error.to_string())
            .and_then(|probe| assert_media_only(probe, "RTMP egress"));
        let rtmp_passed = rtmp_validation.is_ok();
        let rtmp_error = rtmp_validation.err();
        let rtmp_streams = rtmp_result
            .as_ref()
            .ok()
            .and_then(|v| normalized_streams(v).ok());

        results["rtmpEgress"] = json!({
            "passed": rtmp_passed,
            "egressUrl": rtmp_egress_url,
            "readUrl": rtmp_read_url,
            "probe": rtmp_result.ok(),
            "normalizedStreams": rtmp_streams,
            "error": rtmp_error,
        });

        // SRT read validation via ffprobe/ffmpeg is unreliable in-process because
        // the SRT publish handler uses blocking srt_recv() in the async runtime,
        // which stalls the play handler's Notify-based wakeup. Instead, validate
        // that the egress sent a reasonable amount of TS data (at least one keyframe).
        let srt_bytes = engine.egress_bytes("out-srt").await;
        let srt_enough_data = srt_bytes > 1_000_000;
        let srt_error = if srt_enough_data {
            None
        } else {
            Some(format!("SRT egress only sent {srt_bytes} bytes"))
        };
        results["srtEgress"] = json!({
            "passed": srt_enough_data,
            "egressUrl": srt_egress_url,
            "egressBytes": srt_bytes,
            "error": srt_error,
        });
    }

    let rtmp_bytes = engine.egress_bytes("out-rtmp").await;
    let srt_bytes = engine.egress_bytes("out-srt").await;
    results["rtmpEgressBytes"] = json!(rtmp_bytes);
    results["srtEgressBytes"] = json!(srt_bytes);

    // Start recording, let it accumulate data, then validate with ffprobe
    let rec_dir = artifact_path("recording-test");
    std::fs::create_dir_all(&rec_dir).map_err(|e| e.to_string())?;
    let rec_token = engine.register_recording("pipe-src").await;
    let rec_ring = engine.get_or_create_pipeline("pipe-src").await;
    let rec_engine = engine.clone();
    let rec_pid = "pipe-src".to_string();
    let rec_dir_c = rec_dir.clone();
    let rec_task = tokio::spawn(async move {
        restream::media::recording::start_recording(
            "egress-test".to_string(),
            rec_pid,
            rec_dir_c.to_string_lossy().to_string(),
            rec_ring,
            rec_engine,
            rec_token,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_secs(6)).await;
    engine.unregister_recording("pipe-src").await;
    let _ = tokio::time::timeout(Duration::from_secs(10), rec_task).await;

    // Find the recording file and run ffprobe
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
            let _ = std::fs::remove_file(path);
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
        None => json!({"passed": false, "error": "recording file not found"}),
    };
    results["recording"] = recording_result;

    let passed = results["rtmpEgress"]["passed"].as_bool().unwrap_or(false)
        && results["srtEgress"]["passed"].as_bool().unwrap_or(false)
        && results["recording"]["passed"].as_bool().unwrap_or(false);
    results["passed"] = json!(passed);

    // Write results and exit immediately. OS threads hold FFmpeg/SRT C
    // contexts whose destructors race with Rust drop ordering and segfault.
    let path = artifact_path("egress.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    println!("artifact={}", path.display());
    // Use _exit to bypass atexit handlers — FFmpeg's atexit codec
    // deregistration can deadlock with OS threads still holding locks.
    unsafe { libc::_exit(if passed { 0 } else { 1 }) };
}

/// Test: SRT ingest of H.265 → RTMP egress with inline H.265→H.264 transcoding.
///
/// Validates that the RTMP output stream contains valid H.264 video + AAC audio
/// (proving the transcoder works correctly end-to-end).
async fn hevc_rtmp_egress_correctness() -> Result<Value, String> {
    let db_path = artifact_path("correctness-hevc-rtmp.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;

    for (id, name, key) in [
        ("pipe-hevc-src", "H.265 SRT source", "e2e-hevc"),
        ("pipe-hevc-sink", "H.264 RTMP sink", "e2e-hevc-sink"),
    ] {
        db::create_pipeline(&pool, id, name, key, None, None)
            .await
            .map_err(|e| e.to_string())?;
    }

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let _rtmp_task = tokio::spawn(start_rtmp_server_on(
        pool.clone(),
        security.clone(),
        engine.clone(),
        RTMP_PORT,
    ));
    let srt_server = Arc::new(SrtServer::new(pool.clone(), engine.clone(), security));
    let _srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("correctness-h265.ts");
    if !fixture.exists() {
        generate_fixture_h265(&fixture).await?;
    }

    let _publisher = spawn_publisher(
        &fixture,
        &format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-hevc&pkt_size=1316"),
        "mpegts",
        true,
    )
    .await?;
    wait_for_ingests(&engine, &["pipe-hevc-src"], Duration::from_secs(15)).await?;
    println!("[hevc-rtmp] Source ingest established (H.265 via SRT)");

    // Verify the source is actually H.265
    let mut results = json!({});
    {
        let ingests = engine.active_ingests.read().await;
        let v = ingests.get("pipe-hevc-src").and_then(|i| i.video.as_ref());
        let codec = v.map(|v| v.codec.as_str()).unwrap_or("unknown");
        println!("[hevc-rtmp] Source video codec: {codec}");
        if codec != "hevc" {
            results["sourceCodec"] = json!(codec);
            results["passed"] = json!(false);
            results["error"] = json!("source codec is not hevc");
            let path = artifact_path("correctness-hevc-rtmp.json");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&results).unwrap());
            unsafe { libc::_exit(1) };
        }
    }

    let source_ring = engine.get_or_create_pipeline("pipe-hevc-src").await;
    let h264_ring = engine
        .get_or_create_h264_transcoder("pipe-hevc-src", "source", source_ring)
        .await;
    let rtmp_sink_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-hevc-sink");

    let egress_token = engine
        .register_egress("out-hevc-rtmp", "pipe-hevc-src", &rtmp_sink_url)
        .await;
    let _rtmp_egress = tokio::spawn(start_rtmp_egress(
        "out-hevc-rtmp".to_string(),
        "pipe-hevc-src".to_string(),
        rtmp_sink_url.clone(),
        h264_ring,
        engine.clone(),
        egress_token.clone(),
    ));

    // Wait for the RTMP egress to connect and publish to the sink pipeline.
    // The egress re-publishes to `rtmp://127.0.0.1:11935/live/e2e-hevc-sink`,
    // which the local RTMP server accepts as a new ingest on pipe-hevc-sink.
    match wait_for_ingests(&engine, &["pipe-hevc-sink"], Duration::from_secs(15)).await {
        Ok(()) => println!("[hevc-rtmp] Sink ingest established (H.264 via RTMP egress)"),
        Err(e) => {
            results["passed"] = json!(false);
            results["error"] = json!(format!("sink ingest never appeared: {e}"));
            results["rtmpEgressBytes"] = json!(engine.egress_bytes("out-hevc-rtmp").await);
            let path = artifact_path("correctness-hevc-rtmp.json");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&results).unwrap());
            unsafe { libc::_exit(1) };
        }
    }

    // Let enough data accumulate for ffprobe to get a valid stream probe
    tokio::time::sleep(Duration::from_secs(3)).await;

    let rtmp_read_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-hevc-sink");
    let rtmp_probe = ffprobe(&rtmp_read_url).await;

    match rtmp_probe {
        Ok(probe) => {
            let media_check = assert_media_only(&probe, "HEVC→RTMP egress");
            let streams = normalized_streams(&probe).ok();

            // The key assertion: video codec must be H.264 (transcoded from H.265)
            let video_h264 = probe["streams"]
                .as_array()
                .and_then(|streams| {
                    streams
                        .iter()
                        .find(|s| s["codec_type"] == "video")
                        .map(|s| s["codec_name"].as_str())
                })
                .flatten()
                == Some("h264");
            let audio_aac = probe["streams"]
                .as_array()
                .and_then(|streams| {
                    streams
                        .iter()
                        .find(|s| s["codec_type"] == "audio")
                        .map(|s| s["codec_name"].as_str())
                })
                .flatten()
                == Some("aac");

            results["passed"] = json!(media_check.is_ok() && video_h264 && audio_aac);
            results["videoCodec"] = json!(if video_h264 { "h264" } else { "NOT_h264" });
            results["audioCodec"] = json!(if audio_aac { "aac" } else { "NOT_aac" });
            results["mediaCheck"] = json!(media_check.is_ok());
            results["mediaError"] = json!(media_check.err());
            if let Some(ref s) = streams {
                results["streams"] = s.clone();
            }
            results["probe"] = probe;
            results["rtmpEgressBytes"] = json!(engine.egress_bytes("out-hevc-rtmp").await);

            if !video_h264 {
                results["error"] =
                    json!("RTMP output video codec is not H.264 — transcoding failed");
            }
        }
        Err(e) => {
            results["passed"] = json!(false);
            results["error"] = json!(format!("ffprobe failed: {e}"));
            results["rtmpEgressBytes"] = json!(engine.egress_bytes("out-hevc-rtmp").await);
        }
    }

    let passed = results["passed"].as_bool().unwrap_or(false);
    let path = artifact_path("correctness-hevc-rtmp.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    println!("artifact={}", path.display());
    // _exit to avoid FFmpeg/SRT teardown races
    unsafe { libc::_exit(if passed { 0 } else { 1 }) };
}

/// Test: SRT ingest of H.265 → SRT egress passthrough.
///
/// Validates that native SRT egress preserves HEVC video identity while carrying
/// AAC audio, so the H.265 path is not silently mislabeled or transcoded.
async fn hevc_srt_passthrough_correctness() -> Result<Value, String> {
    let db_path = artifact_path("correctness-hevc-srt.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;

    for (id, name, key) in [
        ("pipe-hevc-srt-src", "H.265 SRT source", "e2e-hevc-srt"),
        (
            "pipe-hevc-srt-sink",
            "H.265 SRT passthrough sink",
            "e2e-hevc-srt-sink",
        ),
    ] {
        db::create_pipeline(&pool, id, name, key, None, None)
            .await
            .map_err(|e| e.to_string())?;
    }

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone(), security));
    let _srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("correctness-h265.ts");
    if !fixture.exists() {
        generate_fixture_h265(&fixture).await?;
    }

    let _publisher = spawn_publisher(
        &fixture,
        &format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-hevc-srt&pkt_size=1316"),
        "mpegts",
        true,
    )
    .await?;
    wait_for_ingests(&engine, &["pipe-hevc-srt-src"], Duration::from_secs(15)).await?;
    println!("[hevc-srt] Source ingest established (H.265 via SRT)");

    let source_ring = engine.get_or_create_pipeline("pipe-hevc-srt-src").await;
    let srt_sink_url =
        format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-hevc-srt-sink&pkt_size=1316");
    let egress_token = engine
        .register_egress("out-hevc-srt", "pipe-hevc-srt-src", &srt_sink_url)
        .await;
    let _srt_egress = tokio::spawn(start_srt_egress(
        "out-hevc-srt".to_string(),
        "pipe-hevc-srt-src".to_string(),
        "source".to_string(),
        srt_sink_url,
        source_ring,
        engine.clone(),
        egress_token,
    ));

    wait_for_ingests(&engine, &["pipe-hevc-srt-sink"], Duration::from_secs(15)).await?;
    println!("[hevc-srt] Sink ingest established (H.265 via SRT egress)");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let srt_read_url = format!(
        "srt://127.0.0.1:{SRT_PORT}?streamid=read:live/e2e-hevc-srt-sink&mode=caller&transtype=live&latency=100"
    );
    let probe = ffprobe(&srt_read_url).await?;
    let media_check = assert_media_only(&probe, "HEVC SRT passthrough");
    let streams = normalized_streams(&probe).ok();
    let video_hevc = probe["streams"]
        .as_array()
        .and_then(|streams| {
            streams
                .iter()
                .find(|s| s["codec_type"] == "video")
                .map(|s| s["codec_name"].as_str())
        })
        .flatten()
        .is_some_and(|codec| codec == "hevc" || codec == "h265");
    let audio_aac = probe["streams"]
        .as_array()
        .and_then(|streams| {
            streams
                .iter()
                .find(|s| s["codec_type"] == "audio")
                .map(|s| s["codec_name"].as_str())
        })
        .flatten()
        == Some("aac");

    let mut results = json!({
        "passed": media_check.is_ok() && video_hevc && audio_aac,
        "videoCodec": if video_hevc { "hevc" } else { "NOT_hevc" },
        "audioCodec": if audio_aac { "aac" } else { "NOT_aac" },
        "mediaCheck": media_check.is_ok(),
        "mediaError": media_check.err(),
        "probe": probe,
        "srtEgressBytes": engine.egress_bytes("out-hevc-srt").await,
    });
    if let Some(s) = streams {
        results["streams"] = s;
    }
    if !video_hevc {
        results["error"] = json!("SRT output video codec is not HEVC — passthrough failed");
    }

    let path = artifact_path("correctness-hevc-srt.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    println!("artifact={}", path.display());
    if results["passed"].as_bool().unwrap_or(false) {
        Ok(results)
    } else {
        Err(format!("HEVC SRT passthrough failed: {results}"))
    }
}

async fn hevc_load_test() -> Result<Value, String> {
    use tokio::fs;

    const LOAD_MEDIAMTX_RTMP: u16 = 11936;
    const LOAD_SRT_PORT: u16 = 11080;

    let pool = db::create_pool("sqlite::memory:")
        .await
        .map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;

    db::create_pipeline(
        &pool,
        "pipe-hevc-load",
        "H.265 load source",
        "hevc-load",
        None,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Start mediamtx on 11936
    let mediamtx_yml = "/tmp/mediamtx-load.yml";
    fs::write(
        mediamtx_yml,
        &format!(
            "rtmp: yes\nrtmpAddress: :{}\nrtsp: no\nhls: no\nwebrtc: no\nsrt: no\napi: no\n",
            LOAD_MEDIAMTX_RTMP
        ),
    )
    .await
    .map_err(|e| e.to_string())?;
    let mediamtx = Command::new("mediamtx")
        .arg(mediamtx_yml)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Start SRT server only (no RTMP server — egresses go to mediamtx)
    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let srt_server = Arc::new(SrtServer::new(pool.clone(), engine.clone(), security));
    let _srt_task = tokio::spawn(srt_server.run(LOAD_SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish H.265 via ffmpeg lavfi → SRT (no fixture file, generated on-the-fly)
    let _publisher = spawn_h265_generator(&format!(
        "srt://127.0.0.1:{LOAD_SRT_PORT}?streamid=publish:live/hevc-load&pkt_size=1316"
    ))
    .await?;
    wait_for_ingests(&engine, &["pipe-hevc-load"], Duration::from_secs(30)).await?;
    println!("[hevc-load] Source ingest established (H.265 via SRT)");

    // Create shared H.264 transcoder
    let source_ring = engine.get_or_create_pipeline("pipe-hevc-load").await;
    let _h264_ring = engine
        .get_or_create_h264_transcoder("pipe-hevc-load", "source", source_ring)
        .await;
    tokio::time::sleep(Duration::from_secs(3)).await; // let first keyframe through encoder

    // Helpers for resource monitoring
    let read_proc_status = |field: &str| -> Option<u64> {
        let content = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in content.lines() {
            if let Some(val) = line.strip_prefix(field) {
                // e.g. "VmRSS:\t123456 kB" or "Threads:\t42"
                let val = val.trim_start().trim_end_matches(" kB").trim();
                return val.parse::<u64>().ok();
            }
        }
        None
    };

    let snapshot_resources = || -> Value {
        let rss_kb = read_proc_status("VmRSS:").unwrap_or(0);
        let threads = read_proc_status("Threads:").unwrap_or(0);
        let transcoder_count = {
            let buffers = engine.transcoder_buffers.try_read().ok();
            buffers.map(|b| b.len()).unwrap_or(0)
        };
        json!({
            "rssKb": rss_kb,
            "threads": threads,
            "transcoderCount": transcoder_count,
        })
    };

    let egress_total = 100;
    let batch_size = 10;
    let mut snapshots: Vec<Value> = Vec::new();

    // Baseline snapshot
    snapshots.push(json!({
        "egressCount": 0,
        "resources": snapshot_resources(),
    }));

    let rtmp_base = format!("rtmp://127.0.0.1:{LOAD_MEDIAMTX_RTMP}/live");

    for batch in 0..(egress_total / batch_size) {
        let start = batch * batch_size;
        let end = (start + batch_size).min(egress_total);
        print!("[hevc-load] Adding egresses {start}..{end} ... ");

        let mut _handles = Vec::with_capacity(batch_size);
        for i in start..end {
            let egress_id = format!("hevc-load-rtmp-{i}");
            let url = format!("{rtmp_base}/hevc-out-{i}");
            let token = engine
                .register_egress(&egress_id, "pipe-hevc-load", &url)
                .await;
            _handles.push(tokio::spawn(start_rtmp_egress(
                egress_id,
                "pipe-hevc-load".to_string(),
                url,
                _h264_ring.clone(),
                engine.clone(),
                token,
            )));
        }

        // Wait for connections to establish
        tokio::time::sleep(Duration::from_secs(3)).await;

        snapshots.push(json!({
            "egressCount": end,
            "resources": snapshot_resources(),
        }));
        println!("{}", snapshots.last().unwrap()["resources"]);
    }

    // Let everything stabilize
    tokio::time::sleep(Duration::from_secs(5)).await;
    snapshots.push(json!({
        "egressCount": egress_total,
        "resources": snapshot_resources(),
    }));

    let final_rss = snapshots.last().unwrap()["resources"]["rssKb"]
        .as_u64()
        .unwrap_or(0);
    let rss_delta = final_rss - snapshots[0]["resources"]["rssKb"].as_u64().unwrap_or(0);
    let max_transcoder_count = snapshots
        .iter()
        .filter_map(|s| s["resources"]["transcoderCount"].as_u64())
        .max()
        .unwrap_or(0);
    let shared = max_transcoder_count == 1;

    let results = json!({
        "passed": shared,
        "sharedTranscoder": shared,
        "maxTranscoderCount": max_transcoder_count,
        "egressTotal": egress_total,
        "rssDeltaKb": rss_delta,
        "rssFinalKb": final_rss,
        "snapshots": snapshots,
    });

    let path = artifact_path("hevc-load.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    println!("artifact={}", path.display());

    drop(mediamtx);
    // _exit to avoid FFmpeg/SRT teardown races
    unsafe { libc::_exit(if shared { 0 } else { 1 }) };
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
async fn spawn_h265_generator(url: &str) -> Result<Child, String> {
    let audio_source = Path::new("media/colorbar-timer.mp4");
    if !audio_source.exists() {
        return Err(format!(
            "audio source not found: {}",
            audio_source.display()
        ));
    }
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
        "testsrc2=size=3840x2160:rate=60",
        "-stream_loop",
        "-1",
        "-i",
    ]);
    cmd.arg(audio_source);
    cmd.args(["-map", "0:v"]);
    for i in 0..16 {
        cmd.args(["-map", &format!("1:a:{i}")]);
    }
    cmd.args([
        "-c:v",
        "libx265",
        "-preset",
        "fast",
        "-g",
        "120",
        "-bf",
        "0",
        "-x265-params",
        "log-level=error",
        "-c:a",
        "copy",
        "-f",
        "mpegts",
    ]);
    cmd.arg(url);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
}

async fn wait_for_ingests(
    engine: &MediaEngine,
    pipeline_ids: &[&str],
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let ingests = engine.active_ingests.read().await;
        let ready = pipeline_ids.iter().all(|id| {
            ingests
                .get(*id)
                .is_some_and(|ingest| ingest.video.is_some() && ingest.audio.is_some())
        });
        drop(ingests);
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for RTMP and SRT metadata".to_string());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

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

async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn in_process_load(readers: usize, packets: usize) -> Result<Value, String> {
    let ring = Arc::new(RingBuffer::new(packets.next_power_of_two() * 2));
    let barrier = Arc::new(Barrier::new(readers + 1));
    let payload = Bytes::from(vec![0x5a; 1_316]);
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(readers);

    for i in 0..readers {
        let mut reader = Reader::new(format!("load_reader_{}", i), ring.clone());
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut count = 0usize;
            let mut bytes = 0usize;
            let mut checksum = 0i64;
            while count < packets {
                match reader.pull() {
                    Ok(Some(packet)) => {
                        count += 1;
                        bytes += packet.payload.len();
                        checksum = checksum.wrapping_add(packet.pts ^ packet.dts);
                    }
                    Ok(None) => tokio::task::yield_now().await,
                    Err(error) => return Err(error.to_string()),
                }
            }
            Ok::<_, String>((count, bytes, checksum))
        }));
    }

    barrier.wait().await;
    let push_started = Instant::now();
    for index in 0..packets {
        ring.push(MediaPacket {
            media_type: if index % 3 == 0 {
                MediaType::Audio
            } else {
                MediaType::Video
            },
            track_index: 0,
            pts: index as i64 * 20,
            dts: index as i64 * 20,
            is_keyframe: index % 60 == 0,
            format: PayloadFormat::Raw,
            payload: payload.clone(),
        });
    }

    let mut delivered_packets = 0usize;
    let mut delivered_bytes = 0usize;
    for task in tasks {
        let (count, bytes, _) = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .map_err(|_| "in-process reader timed out".to_string())?
            .map_err(|e| e.to_string())??;
        delivered_packets += count;
        delivered_bytes += bytes;
    }
    let elapsed = push_started.elapsed();
    let total_elapsed = started.elapsed();
    let expected_deliveries = readers * packets;
    if delivered_packets != expected_deliveries {
        return Err(format!(
            "expected {expected_deliveries} deliveries, got {delivered_packets}"
        ));
    }

    Ok(json!({
        "passed": true,
        "readers": readers,
        "sourcePackets": packets,
        "payloadBytes": payload.len(),
        "deliveredPackets": delivered_packets,
        "deliveredBytes": delivered_bytes,
        "fanoutDeliveriesPerSecond": delivered_packets as f64 / elapsed.as_secs_f64(),
        "sourcePacketsPerSecond": packets as f64 / elapsed.as_secs_f64(),
        "deliveryElapsedMs": elapsed.as_secs_f64() * 1000.0,
        "totalElapsedMs": total_elapsed.as_secs_f64() * 1000.0,
    }))
}

#[derive(Default)]
struct SinkMetrics {
    connections: AtomicUsize,
    publishing: AtomicUsize,
    messages: AtomicU64,
    bytes: AtomicU64,
}

async fn network_load(connections: usize, duration: Duration) -> Result<Value, String> {
    let metrics = Arc::new(SinkMetrics::default());
    let sink_metrics = metrics.clone();
    let listener = TcpListener::bind(("127.0.0.1", SINK_PORT))
        .await
        .map_err(|e| e.to_string())?;
    let sink_task = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let metrics = sink_metrics.clone();
            tokio::spawn(async move {
                let _ = handle_sink_client(socket, metrics).await;
            });
        }
    });

    let engine = Arc::new(MediaEngine::new());
    let ring = Arc::new(RingBuffer::new(4_096));
    let mut tokens = Vec::with_capacity(connections);
    let mut tasks = Vec::with_capacity(connections);
    for index in 0..connections {
        let output_id = format!("load-{index}");
        let url = format!("rtmp://127.0.0.1:{SINK_PORT}/live/{output_id}");
        let token = engine
            .register_egress(&output_id, "load-pipeline", &url)
            .await;
        tokens.push(token.clone());
        let pid = "load-pipeline".to_string();
        tasks.push(tokio::spawn(start_rtmp_egress(
            output_id,
            pid,
            url,
            ring.clone(),
            engine.clone(),
            token,
        )));
    }

    wait_for_count(&metrics.publishing, connections, Duration::from_secs(10)).await?;
    let started = Instant::now();
    let payload = Bytes::from(vec![0x33; 1_024]);
    let mut source_packets = 0usize;
    while started.elapsed() < duration {
        let frame = source_packets;
        ring.push(MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: frame as i64 * 33,
            dts: frame as i64 * 33,
            is_keyframe: frame.is_multiple_of(60),
            format: PayloadFormat::Flv,
            payload: Bytes::from(
                [
                    if frame.is_multiple_of(60) { 0x17 } else { 0x27 },
                    0x01,
                    0,
                    0,
                    0,
                ]
                .into_iter()
                .chain(payload.iter().copied())
                .collect::<Vec<_>>(),
            ),
        });
        ring.push(MediaPacket {
            media_type: MediaType::Audio,
            track_index: 0,
            pts: frame as i64 * 33,
            dts: frame as i64 * 33,
            is_keyframe: false,
            format: PayloadFormat::Flv,
            payload: Bytes::from(
                [0xaf, 0x01]
                    .into_iter()
                    .chain(payload[..256].iter().copied())
                    .collect::<Vec<_>>(),
            ),
        });
        source_packets += 2;
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    for token in &tokens {
        token.cancel();
    }
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }
    sink_task.abort();

    let received_messages = metrics.messages.load(Ordering::Relaxed);
    let received_bytes = metrics.bytes.load(Ordering::Relaxed);
    if received_messages == 0 || received_bytes == 0 {
        return Err("network sink received no media".to_string());
    }

    Ok(json!({
        "passed": true,
        "connectionsRequested": connections,
        "connectionsAccepted": metrics.connections.load(Ordering::Relaxed),
        "publishersAccepted": metrics.publishing.load(Ordering::Relaxed),
        "sourcePackets": source_packets,
        "sinkMessages": received_messages,
        "sinkBytes": received_bytes,
        "durationSeconds": duration.as_secs_f64(),
        "sinkMessagesPerSecond": received_messages as f64 / duration.as_secs_f64(),
        "sinkMbps": received_bytes as f64 * 8.0 / duration.as_secs_f64() / 1_000_000.0,
    }))
}

async fn wait_for_count(
    value: &AtomicUsize,
    expected: usize,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while value.load(Ordering::Acquire) < expected {
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {expected} sessions; got {}",
                value.load(Ordering::Relaxed)
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn handle_sink_client(
    mut socket: TcpStream,
    metrics: Arc<SinkMetrics>,
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
    write_sink_results(&mut socket, &mut session, initial, &metrics).await?;
    if !remaining.is_empty() {
        let results = session
            .handle_input(&remaining)
            .map_err(|e| format!("{e:?}"))?;
        write_sink_results(&mut socket, &mut session, results, &metrics).await?;
    }

    loop {
        let n = socket.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        let results = session
            .handle_input(&buffer[..n])
            .map_err(|e| format!("{e:?}"))?;
        write_sink_results(&mut socket, &mut session, results, &metrics).await?;
    }
}

async fn write_sink_results(
    socket: &mut TcpStream,
    session: &mut ServerSession,
    results: Vec<ServerSessionResult>,
    metrics: &SinkMetrics,
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
                ServerSessionEvent::VideoDataReceived { data, .. }
                | ServerSessionEvent::AudioDataReceived { data, .. } => {
                    metrics.messages.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

async fn matrix_correctness() -> Result<Value, String> {
    let db_path = artifact_path("matrix.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;

    // Create 4 source pipelines + 8 sink pipelines
    let pipelines = [
        ("pipe-rtmp-direct", "RTMP Direct Source", "e2e-rtmp-direct"),
        (
            "pipe-rtmp-trans",
            "RTMP Transcoded Source",
            "e2e-rtmp-trans",
        ),
        ("pipe-srt-direct", "SRT Direct Source", "e2e-srt-direct"),
        ("pipe-srt-trans", "SRT Transcoded Source", "e2e-srt-trans"),
        (
            "pipe-rtmp-rtmp-direct-sink",
            "RTMP->RTMP Direct Sink",
            "e2e-rtmp-rtmp-direct-sink",
        ),
        (
            "pipe-rtmp-rtmp-trans-sink",
            "RTMP->RTMP Trans Sink",
            "e2e-rtmp-rtmp-trans-sink",
        ),
        (
            "pipe-rtmp-srt-direct-sink",
            "RTMP->SRT Direct Sink",
            "e2e-rtmp-srt-direct-sink",
        ),
        (
            "pipe-rtmp-srt-trans-sink",
            "RTMP->SRT Trans Sink",
            "e2e-rtmp-srt-trans-sink",
        ),
        (
            "pipe-srt-rtmp-direct-sink",
            "SRT->RTMP Direct Sink",
            "e2e-srt-rtmp-direct-sink",
        ),
        (
            "pipe-srt-rtmp-trans-sink",
            "SRT->RTMP Trans Sink",
            "e2e-srt-rtmp-trans-sink",
        ),
        (
            "pipe-srt-srt-direct-sink",
            "SRT->SRT Direct Sink",
            "e2e-srt-srt-direct-sink",
        ),
        (
            "pipe-srt-srt-trans-sink",
            "SRT->SRT Trans Sink",
            "e2e-srt-srt-trans-sink",
        ),
    ];

    for (id, name, key) in pipelines {
        db::create_pipeline(&pool, id, name, key, None, None)
            .await
            .map_err(|e| e.to_string())?;
    }

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));

    // Spawn servers
    let _rtmp_task = tokio::spawn(start_rtmp_server_on(
        pool.clone(),
        security.clone(),
        engine.clone(),
        RTMP_PORT,
    ));
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone(), security));
    let _srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check fixtures
    let rtmp_fixture = artifact_path("correctness-h264.ts");
    if !rtmp_fixture.exists() {
        generate_fixture_h264(&rtmp_fixture).await?;
    }
    let srt_fixture = artifact_path("correctness-h265.ts");
    if !srt_fixture.exists() {
        generate_fixture_h265(&srt_fixture).await?;
    }

    // Set up direct and transcoded outputs manually
    let mut egress_handles = Vec::new();
    let mut transcode_handles = Vec::new();
    let mut hls_handles = Vec::new();

    let paths = [
        // (src_pipe, is_transcoded, sink_rtmp_key, sink_srt_key)
        (
            "pipe-rtmp-direct",
            false,
            "e2e-rtmp-rtmp-direct-sink",
            "e2e-rtmp-srt-direct-sink",
        ),
        (
            "pipe-rtmp-trans",
            true,
            "e2e-rtmp-rtmp-trans-sink",
            "e2e-rtmp-srt-trans-sink",
        ),
        (
            "pipe-srt-direct",
            false,
            "e2e-srt-rtmp-direct-sink",
            "e2e-srt-srt-direct-sink",
        ),
        (
            "pipe-srt-trans",
            true,
            "e2e-srt-rtmp-trans-sink",
            "e2e-srt-srt-trans-sink",
        ),
    ];

    for (src_pipe, trans, rtmp_sink_key, srt_sink_key) in paths {
        let source_ring = engine.get_or_create_pipeline(src_pipe).await;
        let target_ring = if trans {
            let trans_ring = Arc::new(RingBuffer::new(4096));
            let cancel = tokio_util::sync::CancellationToken::new();
            transcode_handles.push(cancel.clone());
            tokio::spawn(restream::media::transcoder::start_transcoder(
                src_pipe.to_string(),
                "720p".to_string(),
                source_ring,
                trans_ring.clone(),
                engine.clone(),
                cancel,
            ));
            trans_ring
        } else {
            source_ring
        };

        // 1. Spawn RTMP egress
        let rtmp_egress_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/{rtmp_sink_key}");
        let rtmp_token = engine
            .register_egress(&format!("out-rtmp-{src_pipe}"), src_pipe, &rtmp_egress_url)
            .await;
        egress_handles.push(rtmp_token.clone());
        tokio::spawn(start_rtmp_egress(
            format!("out-rtmp-{src_pipe}"),
            src_pipe.to_string(),
            rtmp_egress_url,
            target_ring.clone(),
            engine.clone(),
            rtmp_token,
        ));

        // 2. Spawn SRT egress
        let srt_egress_url = format!(
            "srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/{srt_sink_key}&pkt_size=1316"
        );
        let srt_token = engine
            .register_egress(&format!("out-srt-{src_pipe}"), src_pipe, &srt_egress_url)
            .await;
        egress_handles.push(srt_token.clone());
        tokio::spawn(start_srt_egress(
            format!("out-srt-{src_pipe}"),
            src_pipe.to_string(),
            "source".to_string(),
            srt_egress_url,
            target_ring.clone(),
            engine.clone(),
            srt_token,
        ));

        // 3. Spawn HLS segmenter
        let (store, _) = engine.ensure_hls_segmenter(src_pipe).await;
        let hls_cancel = engine.get_hls_cancel_token(src_pipe).await.unwrap();
        hls_handles.push(hls_cancel.clone());
        tokio::spawn(restream::media::hls::start_hls_segmenter(
            src_pipe.to_string(),
            store,
            target_ring,
            engine.clone(),
            hls_cancel,
        ));
        engine.add_hls_persistent_consumer(src_pipe).await;
    }

    // Now spawn the 4 publishers pushing feeds
    let mut publishers = Vec::new();

    // pipe-rtmp-direct: RTMP H.264
    publishers.push(
        spawn_publisher(
            &rtmp_fixture,
            &format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp-direct"),
            "flv",
            false,
        )
        .await?,
    );

    // pipe-rtmp-trans: RTMP H.265 (to test HEVC transcode to H.264)
    publishers.push(
        spawn_publisher(
            &srt_fixture, // H.265 fixture
            &format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp-trans"),
            "flv",
            false,
        )
        .await?,
    );

    // pipe-srt-direct: SRT H.264
    publishers.push(
        spawn_publisher(
            &rtmp_fixture,
            &format!(
                "srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt-direct&pkt_size=1316"
            ),
            "mpegts",
            true,
        )
        .await?,
    );

    // pipe-srt-trans: SRT H.265
    publishers.push(
        spawn_publisher(
            &srt_fixture, // H.265 fixture
            &format!(
                "srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt-trans&pkt_size=1316"
            ),
            "mpegts",
            true,
        )
        .await?,
    );

    println!("[matrix] Waiting for publishers and egress loopbacks to start...");
    let source_pipes = [
        "pipe-rtmp-direct",
        "pipe-rtmp-trans",
        "pipe-srt-direct",
        "pipe-srt-trans",
    ];
    wait_for_ingests(&engine, &source_pipes, Duration::from_secs(15)).await?;

    let sink_pipes = [
        "pipe-rtmp-rtmp-direct-sink",
        "pipe-rtmp-rtmp-trans-sink",
        "pipe-rtmp-srt-direct-sink",
        "pipe-rtmp-srt-trans-sink",
        "pipe-srt-rtmp-direct-sink",
        "pipe-srt-rtmp-trans-sink",
        "pipe-srt-srt-direct-sink",
        "pipe-srt-srt-trans-sink",
    ];
    wait_for_ingests(&engine, &sink_pipes, Duration::from_secs(20)).await?;

    println!("[matrix] All streams active. Probing egress endpoints...");

    let mut results = json!({ "passed": true });

    // Validate 8 Loopback Sinks
    for sink in sink_pipes {
        let (read_url, _is_rtmp) = if sink.contains("-rtmp-") {
            let key = sink
                .strip_prefix("pipe-")
                .unwrap()
                .strip_suffix("-sink")
                .unwrap();
            (
                format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-{key}-sink"),
                true,
            )
        } else {
            let key = sink
                .strip_prefix("pipe-")
                .unwrap()
                .strip_suffix("-sink")
                .unwrap();
            (
                format!(
                    "srt://127.0.0.1:{SRT_PORT}?streamid=read:live/e2e-{key}-sink&mode=caller&transtype=live&latency=100"
                ),
                false,
            )
        };

        match ffprobe(&read_url).await {
            Ok(probe) => {
                if let Err(e) = assert_media_only(&probe, sink) {
                    results["passed"] = json!(false);
                    results[sink] = json!({ "passed": false, "error": e });
                } else {
                    results[sink] =
                        json!({ "passed": true, "streams": normalized_streams(&probe)? });
                }
            }
            Err(e) => {
                results["passed"] = json!(false);
                results[sink] = json!({ "passed": false, "error": e });
            }
        }
    }

    // Validate 4 HLS Egresses
    for src in source_pipes {
        let store_opt = engine.get_hls_store(src).await;
        if let Some(store) = store_opt {
            if let Some(playlist) = store.get_playlist() {
                if playlist.contains(".ts") && store.get_segment(0).is_some() {
                    results[&format!("{src}-hls")] = json!({ "passed": true, "segment_count": 1 });
                } else {
                    results["passed"] = json!(false);
                    results[&format!("{src}-hls")] =
                        json!({ "passed": false, "error": "playlist empty or segment 0 missing" });
                }
            } else {
                results["passed"] = json!(false);
                results[&format!("{src}-hls")] =
                    json!({ "passed": false, "error": "playlist not found" });
            }
        } else {
            results["passed"] = json!(false);
            results[&format!("{src}-hls")] =
                json!({ "passed": false, "error": "HLS store not created" });
        }
    }

    // Stop all publishers and clean up
    for mut pub_proc in publishers {
        stop_child(&mut pub_proc).await;
    }
    for token in egress_handles {
        token.cancel();
    }
    for cancel in transcode_handles {
        cancel.cancel();
    }
    for cancel in hls_handles {
        cancel.cancel();
    }

    Ok(results)
}

async fn matrix_correctness_in_memory() -> Result<Value, String> {
    let cases = [
        ("rtmp", "rtmp", false),
        ("rtmp", "rtmp", true),
        ("rtmp", "srt", false),
        ("rtmp", "srt", true),
        ("rtmp", "hls", false),
        ("rtmp", "hls", true),
        ("srt", "rtmp", false),
        ("srt", "rtmp", true),
        ("srt", "srt", false),
        ("srt", "srt", true),
        ("srt", "hls", false),
        ("srt", "hls", true),
    ];

    let mut results = serde_json::Map::new();
    let mut all_passed = true;

    for (ingest, egress, trans) in cases {
        let name = format!(
            "{}_to_{}_{}",
            ingest,
            egress,
            if trans { "trans" } else { "direct" }
        );
        println!("[matrix-in-memory] Running case: {}", name);

        let engine = Arc::new(MediaEngine::new());
        let source_ring = engine.get_or_create_pipeline("pipe").await;

        // Register active ingest
        engine
            .try_register_ingest("pipe", "key", ingest)
            .await
            .ok_or_else(|| "Failed to register ingest".to_string())?;

        let fixture_name = if trans {
            "correctness-h265.ts"
        } else {
            "correctness-h264.ts"
        };
        let (video_meta, audio_tracks, packets) = load_fixture_packets(fixture_name, ingest);

        engine
            .update_ingest_meta("pipe", Some(video_meta.clone()), None, None)
            .await;
        engine
            .update_ingest_audio_tracks("pipe", audio_tracks.clone())
            .await;

        let (target_ring, transcoder_cancel, transcoder_handle) = if trans {
            let trans_ring = Arc::new(RingBuffer::new(4096));
            let cancel = tokio_util::sync::CancellationToken::new();
            let handle = tokio::spawn(restream::media::transcoder::start_transcoder(
                "pipe".to_string(),
                "720p".to_string(),
                source_ring.clone(),
                trans_ring.clone(),
                engine.clone(),
                cancel.clone(),
            ));
            (trans_ring, Some(cancel), Some(handle))
        } else {
            (source_ring.clone(), None, None)
        };

        // Create reader BEFORE pushing packets
        let mut reader = Reader::new("test_egress_reader".to_string(), target_ring.clone());

        // Spawn HLS segmenter BEFORE pushing packets
        let hls_segmenter = if egress == "hls" {
            let (store, _) = engine.ensure_hls_segmenter("pipe").await;
            let cancel = tokio_util::sync::CancellationToken::new();
            let segmenter = tokio::spawn(restream::media::hls::start_hls_segmenter(
                "pipe".to_string(),
                store.clone(),
                target_ring.clone(),
                engine.clone(),
                cancel.clone(),
            ));
            Some((store, cancel, segmenter))
        } else {
            None
        };

        // Wait a tiny bit for background tasks to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Prepare test data: push packets
        let num_packets = packets.len();
        for pkt in packets {
            source_ring.push(pkt);
        }

        // Pull and verify
        let mut pulled = 0;
        let mut max_pts: i64 = 0;
        let mut case_passed = false;
        let mut err_msg = String::new();

        if let Some((store, cancel, segmenter)) = hls_segmenter {
            if trans {
                let mut trans_reader =
                    Reader::new("test_trans_reader".to_string(), target_ring.clone());
                let mut trans_pulled = 0;
                let start = Instant::now();
                while trans_pulled < num_packets && start.elapsed() < Duration::from_millis(2000) {
                    if let Ok(Some(_)) = trans_reader.pull() {
                        trans_pulled += 1;
                    } else {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            cancel.cancel();
            let _ = segmenter.await;
            if let Some(playlist) = store.get_playlist() {
                if playlist.contains(".ts") {
                    if let Some(seg_bytes) = store.get_segment(0) {
                        // Write segment to temp file and validate with ffprobe
                        let tmp = std::env::temp_dir().join(format!("hls-{}.ts", name));
                        if std::fs::write(&tmp, seg_bytes.as_ref()).is_ok() {
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
                                        tmp.to_string_lossy().as_ref(),
                                    ])
                                    .output(),
                            )
                            .await;
                            let _ = std::fs::remove_file(&tmp);
                            if let Ok(Ok(out)) = output {
                                if let Ok(probe) = serde_json::from_slice::<Value>(&out.stdout) {
                                    if let Some(streams) = probe["streams"].as_array() {
                                        let has_video =
                                            streams.iter().any(|s| s["codec_type"] == "video");
                                        let has_audio =
                                            streams.iter().any(|s| s["codec_type"] == "audio");
                                        if has_video && has_audio {
                                            case_passed = true;
                                        } else {
                                            err_msg = format!(
                                                "HLS segment missing streams: video={}, audio={}",
                                                has_video, has_audio
                                            );
                                        }
                                    } else {
                                        err_msg = "HLS segment: no streams in ffprobe".to_string();
                                    }
                                } else {
                                    err_msg = "HLS segment: ffprobe parse failed".to_string();
                                }
                            } else {
                                err_msg = "HLS segment: ffprobe failed".to_string();
                            }
                        } else {
                            err_msg = "HLS segment: failed to write temp file".to_string();
                        }
                    } else {
                        err_msg = "HLS segment 0 not found".to_string();
                    }
                } else {
                    err_msg = "HLS playlist missing .ts reference".to_string();
                }
            } else {
                err_msg = "HLS playlist not found".to_string();
            }
        } else if egress == "srt" {
            let mut muxer = restream::media::mpegts::TsMuxer::new(Some(&video_meta), &audio_tracks);

            let num_streams = 1 + audio_tracks.len();
            let mut dts_enforcer = DtsEnforcer::new(num_streams);
            let mut nalu_len_size: usize = 4;
            let mut sps_pps_cache: Vec<u8> = Vec::new();

            let start = Instant::now();
            let mut has_ts_bytes = false;
            while start.elapsed() < Duration::from_millis(1500) && pulled < num_packets {
                if let Ok(Some(pkt)) = reader.pull() {
                    max_pts = max_pts.max(pkt.pts).max(pkt.dts);
                    let payload = match pkt.media_type {
                        MediaType::Video => video_for_ts(
                            &pkt.payload,
                            pkt.format,
                            &mut nalu_len_size,
                            &mut sps_pps_cache,
                        ),
                        MediaType::Audio => {
                            let track = audio_tracks
                                .iter()
                                .find(|a| a.track_index == pkt.track_index)
                                .or(audio_tracks.first());
                            let (sr, ch) = track
                                .map(|a| (a.sample_rate, a.channels))
                                .unwrap_or((48000, 1));
                            audio_for_ts(&pkt.payload, pkt.format, sr, ch)
                        }
                    };
                    if let Some(raw) = payload {
                        let stream_idx = match pkt.media_type {
                            MediaType::Video => 0,
                            MediaType::Audio => audio_tracks
                                .iter()
                                .position(|a| a.track_index == pkt.track_index)
                                .map(|i| i + 1)
                                .unwrap_or(0),
                        };
                        let (pts, dts) = dts_enforcer.enforce(stream_idx, pkt.pts, pkt.dts);
                        let ts_bytes = muxer.mux_packet(
                            pkt.media_type,
                            pkt.track_index,
                            pts,
                            dts,
                            pkt.is_keyframe,
                            &raw,
                        );
                        if !ts_bytes.is_empty() {
                            has_ts_bytes = true;
                        }
                    }
                    pulled += 1;
                } else {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
            if pulled == num_packets && has_ts_bytes {
                if max_pts > 60_000 {
                    case_passed = false;
                    err_msg = format!(
                        "SRT egress failed: timestamps not in ms range (max_pts={}, expected <60000)",
                        max_pts
                    );
                } else {
                    case_passed = true;
                }
            } else {
                err_msg = format!(
                    "SRT egress failed: pulled={}/{}, has_ts_bytes={}",
                    pulled, num_packets, has_ts_bytes
                );
            }
        } else {
            // RTMP Egress
            let start = Instant::now();
            let mut has_video = false;
            let mut has_audio = false;
            while start.elapsed() < Duration::from_millis(1500) && pulled < num_packets {
                if let Ok(Some(pkt)) = reader.pull() {
                    max_pts = max_pts.max(pkt.pts).max(pkt.dts);
                    match pkt.media_type {
                        MediaType::Video => has_video = true,
                        MediaType::Audio => has_audio = true,
                    }
                    pulled += 1;
                } else {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
            if pulled == num_packets && has_video && has_audio {
                if max_pts > 60_000 {
                    case_passed = false;
                    err_msg = format!(
                        "RTMP egress failed: timestamps not in ms range (max_pts={}, expected <60000)",
                        max_pts
                    );
                } else {
                    case_passed = true;
                }
            } else {
                err_msg = format!(
                    "RTMP egress failed: pulled={}/{}, has_video={}, has_audio={}",
                    pulled, num_packets, has_video, has_audio
                );
            }
        }

        if let Some(cancel) = transcoder_cancel {
            cancel.cancel();
        }
        if let Some(handle) = transcoder_handle {
            let _ = handle.await;
        }

        if !case_passed {
            all_passed = false;
        }
        results.insert(
            name,
            json!({
                "passed": case_passed,
                "error": if case_passed { None } else { Some(err_msg) }
            }),
        );
    }

    let mut final_res = serde_json::Value::Object(results);
    final_res["passed"] = json!(all_passed);

    if all_passed {
        Ok(final_res)
    } else {
        Err(format!("matrix correctness failed: {}", final_res))
    }
}

fn load_fixture_packets(
    fixture_name: &str,
    ingest: &str,
) -> (
    restream::media::engine::VideoMeta,
    Vec<restream::media::engine::AudioMeta>,
    Vec<MediaPacket>,
) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest_dir)
        .join("test/artifacts/latest")
        .join(fixture_name);
    let file_bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture at {}: {}", path.display(), e));

    let mut demuxer = restream::media::mpegts::TsDemuxer::new();
    let mut all_packets = Vec::new();

    for chunk in file_bytes.chunks(1316) {
        demuxer.feed(chunk);
        demuxer.drain_into(&mut all_packets);
    }
    demuxer.flush();
    demuxer.drain_into(&mut all_packets);

    let mut probe = demuxer.take_probe().expect("failed to probe TS file");
    let video = probe.video.expect("missing video metadata");

    // Keep only the first audio track
    let mut audio_tracks: Vec<restream::media::engine::AudioMeta> =
        probe.audio_tracks.drain(..).take(1).collect();
    let keep_audio_track_index = audio_tracks.first().map(|a| a.track_index).unwrap_or(0);
    if let Some(a) = audio_tracks.first_mut() {
        a.track_index = 0;
    }

    // Filter packets: keep all video packets, and keep audio packets belonging to track 0
    let mut packets = Vec::new();
    for mut pkt in all_packets {
        if pkt.media_type == MediaType::Video {
            packets.push(pkt);
        } else if pkt.media_type == MediaType::Audio && pkt.track_index == keep_audio_track_index {
            // Re-map audio track index to 0
            pkt.track_index = 0;
            packets.push(pkt);
        }
    }

    // Wrap packets with FLV tags if ingest is RTMP
    if ingest == "rtmp" {
        for pkt in &mut packets {
            let is_video = pkt.media_type == MediaType::Video;
            let mut wrapped = Vec::with_capacity(pkt.payload.len() + 5);
            if is_video {
                let is_hevc = video.codec == "hevc" || video.codec == "h265";
                let tag_byte = if is_hevc {
                    if pkt.is_keyframe { 0x1c } else { 0x2c }
                } else {
                    if pkt.is_keyframe { 0x17 } else { 0x27 }
                };
                wrapped.extend_from_slice(&[tag_byte, 1, 0, 0, 0]);
            } else {
                wrapped.extend_from_slice(&[0xaf, 1]);
            }
            wrapped.extend_from_slice(&pkt.payload);
            pkt.payload = Bytes::from(wrapped);
            pkt.format = PayloadFormat::Flv;
        }
    }

    (video, audio_tracks, packets)
}
