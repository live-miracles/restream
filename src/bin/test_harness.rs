use bytes::Bytes;
use restream::db;
use restream::media::engine::MediaEngine;
use restream::media::ring_buffer::{MediaPacket, MediaType, Reader, RingBuffer};
use restream::media::rtmp::{start_rtmp_egress, start_rtmp_server_on};
use restream::media::security::{DEFAULT_INGEST_SECURITY_CONFIG, IngestSecurityService};
use restream::media::srt::{SrtServer, start_srt_egress};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
};
use serde_json::{Value, json};
use sqlx::SqlitePool;
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
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let result = match command.as_str() {
        "correctness" => correctness().await,
        "egress" => egress_correctness().await,
        "in-process" => in_process_load(500, 2_000).await,
        "network" => network_load(32, Duration::from_secs(5)).await,
        "all" => {
            let correctness = correctness().await?;
            let in_process = in_process_load(500, 2_000).await?;
            let network = network_load(32, Duration::from_secs(5)).await?;
            // egress_correctness calls process::exit to avoid FFmpeg/SRT
            // teardown segfaults, so it must run last.
            let egress = egress_correctness().await?;
            Ok(json!({
                "correctness": correctness,
                "egress": egress,
                "inProcess": in_process,
                "network": network,
            }))
        }
        other => Err(format!(
            "unknown command {other:?}; use correctness, egress, in-process, network, or all"
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
    let pool = SqlitePool::connect(&db_url)
        .await
        .map_err(|e| e.to_string())?;
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
        security,
        engine.clone(),
        RTMP_PORT,
    ));
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone()));
    let srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("correctness-h264.ts");
    generate_fixture(&fixture).await?;

    let mut rtmp_publisher = spawn_publisher(
        &fixture,
        &format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp"),
        "flv",
    )
    .await?;
    let mut srt_publisher = spawn_publisher(
        &fixture,
        &format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt&pkt_size=1316"),
        "mpegts",
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
        "srt://127.0.0.1:{SRT_PORT}?streamid=read:live/e2e-srt&mode=caller"
    ))
    .await?;

    let rtmp_media = normalized_streams(&rtmp_probe)?;
    let srt_media = normalized_streams(&srt_probe)?;
    let matches = rtmp_media == srt_media;

    stop_child(&mut rtmp_publisher).await;
    stop_child(&mut srt_publisher).await;
    rtmp_task.abort();
    srt_task.abort();

    if !matches {
        return Err(format!(
            "normalized probes differ: RTMP={} SRT={}",
            rtmp_media, srt_media
        ));
    }

    Ok(json!({
        "passed": true,
        "fixture": fixture,
        "rtmp": {
            "publishUrl": format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp"),
            "readUrl": format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-rtmp"),
            "snapshot": rtmp_snapshot,
            "probe": rtmp_probe,
        },
        "srt": {
            "publishUrl": format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt"),
            "readUrl": format!("srt://127.0.0.1:{SRT_PORT}?streamid=read:live/e2e-srt&mode=caller"),
            "snapshot": srt_snapshot,
            "probe": srt_probe,
        },
        "normalizedStreams": rtmp_media,
        "probesMatch": matches,
    }))
}

async fn egress_correctness() -> Result<Value, String> {
    let db_path = artifact_path("egress.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = SqlitePool::connect(&db_url)
        .await
        .map_err(|e| e.to_string())?;
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
        security,
        engine.clone(),
        RTMP_PORT,
    ));
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone()));
    let _srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture(&fixture).await?;
    }

    // Retain the publisher process handle so it remains alive for the probes.
    let _publisher = spawn_publisher(
        &fixture,
        &format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-src"),
        "flv",
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
        let rtmp_passed = rtmp_result.is_ok();
        let rtmp_error = rtmp_result.as_ref().err().map(|e| e.to_string());
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

        let srt_read_url =
            format!("srt://127.0.0.1:{SRT_PORT}?streamid=read:live/e2e-srt-sink&mode=caller");
        let srt_result = ffprobe(&srt_read_url).await;
        let srt_passed = srt_result.is_ok();
        let srt_error = srt_result.as_ref().err().map(|e| e.to_string());
        let srt_streams = srt_result
            .as_ref()
            .ok()
            .and_then(|v| normalized_streams(v).ok());

        results["srtEgress"] = json!({
            "passed": srt_passed,
            "egressUrl": srt_egress_url,
            "readUrl": srt_read_url,
            "probe": srt_result.ok(),
            "normalizedStreams": srt_streams,
            "error": srt_error,
        });
    }

    let rtmp_bytes = engine.egress_bytes("out-rtmp").await;
    let srt_bytes = engine.egress_bytes("out-srt").await;
    results["rtmpEgressBytes"] = json!(rtmp_bytes);
    results["srtEgressBytes"] = json!(srt_bytes);

    let passed = results["rtmpEgress"]["passed"].as_bool().unwrap_or(false)
        && results["srtEgress"]["passed"].as_bool().unwrap_or(false);
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

async fn generate_fixture(path: &Path) -> Result<(), String> {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
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
            "veryfast",
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
        Err(format!("fixture generation failed: {status}"))
    }
}

async fn spawn_publisher(path: &Path, url: &str, format: &str) -> Result<Child, String> {
    Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-stream_loop",
            "-1",
            "-i",
        ])
        .arg(path)
        .args(["-map", "0:v", "-map", "0:a:0", "-c", "copy", "-f", format])
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())
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
                "stream=index,codec_name,codec_type,width,height,sample_rate,channels",
                "-of",
                "json",
                url,
            ])
            .output(),
    )
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

    for _ in 0..readers {
        let mut reader = Reader::new(ring.clone());
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
            is_keyframe: frame % 60 == 0,
            payload: Bytes::from(
                [if frame % 60 == 0 { 0x17 } else { 0x27 }, 0x01, 0, 0, 0]
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
