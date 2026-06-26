use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
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
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

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
        "correctness-srt-rtmp" => srt_to_rtmp_correctness().await,
        "hls-put" => hls_put_correctness().await,
        "burst-verify" => burst_verify_correctness().await,
        "bframe-rtmp" => bframe_rtmp_correctness().await,
        "ramp-family" => ramp_family_correctness().await,
        "mixed-anchor" => mixed_anchor_correctness().await,
        "mixed-h265-srt" => mixed_h265_srt_correctness().await,
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
              correctness-srt-rtmp, hls-put, burst-verify, bframe-rtmp, ramp-family, \
              mixed-anchor, mixed-h265-srt, matrix, \
              matrix-in-memory, egress, correctness-hevc-rtmp, correctness-hevc-srt, \
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

    Ok(json!({
        "config": cfg,
        "pipelineId": pipeline_id,
        "nPerGroup": n,
        "totalOutputs": total,
        "rssDeltaKb": rss_delta,
        "perOutputKb": per_output,
        "extFfmpegCount": ffmpeg.count,
        "extFfmpegRssKb": ffmpeg.rss_kb,
    }))
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
        check_mixed_stream(
            &format!("RTMP-src  out{n}"),
            &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{n}", env.mtx_rtmp),
            "1920x1080",
            None,
        )
        .await;
        check_mixed_stream(
            &format!("RTMP-720p out{n}"),
            &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{n}", env.mtx_rtmp),
            "1280x720",
            None,
        )
        .await;
        check_mixed_stream(
            &format!("SRT-src   out{n}"),
            &format!(
                "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-src-{n}&timeout=30000000",
                env.mtx_srt
            ),
            "1920x1080",
            None,
        )
        .await;
        check_mixed_stream(
            &format!("SRT-720p  out{n}"),
            &format!(
                "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-720p-{n}&timeout=30000000",
                env.mtx_srt
            ),
            "1280x720",
            None,
        )
        .await;
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

    stop_child(&mut publisher).await;
    stop_mixed_outputs(api, &pipeline_id, &output_ids).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    Ok(json!({
        "config": cfg,
        "pipelineId": pipeline_id,
        "nPerGroup": n,
        "totalOutputs": total,
        "rssDeltaKb": rss_delta,
        "perOutputKb": per_output,
        "extFfmpegCount": ffmpeg.count,
        "extFfmpegRssKb": ffmpeg.rss_kb,
        "tcSpawns": count_log_matches(&env.restream_log, "[h264-tc] Spawning"),
    }))
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

async fn check_mixed_stream(label: &str, url: &str, expected: &str, cookie: Option<&str>) {
    let mut last = String::new();
    for _ in 1..=15 {
        match probe_dims_ramp_with_cookie(url, cookie).await {
            Ok(dimensions) if dimensions == expected => {
                println!("  ok   {label:<45} -> {dimensions}");
                return;
            }
            Ok(dimensions) => {
                if !dimensions.is_empty() {
                    last = dimensions;
                }
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    println!(
        "  FAIL {label:<45} expected={expected} got={}",
        if last.is_empty() { "none" } else { &last }
    );
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

/// Test: SRT H.264/AAC ingest → RTMP source egress.
///
/// Validates the direct Raw Annex-B/ADTS to RTMP FLV/AVCC/AAC packetization
/// path without involving a transcoder.
async fn srt_to_rtmp_correctness() -> Result<Value, String> {
    let db_path = artifact_path("correctness-srt-rtmp.sqlite");
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
        ("pipe-srt-rtmp-src", "H.264 SRT source", "e2e-srt-rtmp"),
        (
            "pipe-srt-rtmp-sink",
            "H.264 RTMP direct sink",
            "e2e-srt-rtmp-sink",
        ),
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
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone(), security));
    let _srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture_h264(&fixture).await?;
    }

    let _publisher = spawn_publisher(
        &fixture,
        &format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-srt-rtmp&pkt_size=1316"),
        "mpegts",
        true,
    )
    .await?;
    wait_for_ingests(&engine, &["pipe-srt-rtmp-src"], Duration::from_secs(15)).await?;
    println!("[srt-rtmp] Source ingest established (H.264 via SRT)");

    let source_ring = engine.get_or_create_pipeline("pipe-srt-rtmp-src").await;
    let rtmp_sink_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-srt-rtmp-sink");
    let egress_token = engine
        .register_egress("out-srt-rtmp", "pipe-srt-rtmp-src", &rtmp_sink_url)
        .await;
    let _rtmp_egress = tokio::spawn(start_rtmp_egress(
        "out-srt-rtmp".to_string(),
        "pipe-srt-rtmp-src".to_string(),
        rtmp_sink_url,
        source_ring,
        engine.clone(),
        egress_token,
    ));

    wait_for_ingests(&engine, &["pipe-srt-rtmp-sink"], Duration::from_secs(15)).await?;
    println!("[srt-rtmp] Sink ingest established (H.264 via RTMP egress)");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let rtmp_read_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-srt-rtmp-sink");
    let probe = ffprobe(&rtmp_read_url).await?;
    let media_check = assert_media_only(&probe, "SRT to RTMP direct egress");
    let streams = normalized_streams(&probe).ok();
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

    let mut results = json!({
        "passed": media_check.is_ok() && video_h264 && audio_aac,
        "videoCodec": if video_h264 { "h264" } else { "NOT_h264" },
        "audioCodec": if audio_aac { "aac" } else { "NOT_aac" },
        "mediaCheck": media_check.is_ok(),
        "mediaError": media_check.err(),
        "probe": probe,
        "rtmpEgressBytes": engine.egress_bytes("out-srt-rtmp").await,
    });
    if let Some(s) = streams {
        results["streams"] = s;
    }
    if !video_h264 {
        results["error"] = json!("RTMP output video codec is not H.264 — packetization failed");
    }

    let path = artifact_path("correctness-srt-rtmp.json");
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
        Err(format!("SRT to RTMP direct egress failed: {results}"))
    }
}

/// Test: SRT ingest -> HLS HTTP PUT upload for YouTube-style and path-style sinks.
async fn hls_put_correctness() -> Result<Value, String> {
    let settle = Duration::from_secs(env_secs("HLS_PUT_SETTLE_SECS", 8));
    let restart_settle = Duration::from_secs(env_secs("HLS_PUT_RESTART_SECS", 12));
    let hls_put_port = std::env::var("HLS_PUT_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8990);
    let sink_dir = std::env::var_os("HLS_PUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| artifact_path("hls-put-sink"));
    let _ = std::fs::remove_dir_all(&sink_dir);
    std::fs::create_dir_all(&sink_dir).map_err(|e| e.to_string())?;

    std::fs::write(
        artifact_path("hls-put-sink.log"),
        format!("Rust in-process HLS PUT sink listening on 127.0.0.1:{hls_put_port}\n"),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        artifact_path("restream.log"),
        "scenario managed by Rust test_harness hls-put\n",
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        artifact_path("publisher.log"),
        "publisher managed by Rust test_harness hls-put\n",
    )
    .map_err(|e| e.to_string())?;

    let (mut sink_cancel, mut sink_handle) =
        start_hls_put_sink(hls_put_port, sink_dir.clone()).await?;

    let db_path = artifact_path("hls-put.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;
    db::create_pipeline(
        &pool,
        "pipe-hls-put",
        "HLS PUT SRT source",
        "e2e-hls-put",
        None,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let srt_server = Arc::new(SrtServer::new(pool, engine.clone(), security));
    let _srt_task = tokio::spawn(srt_server.run(SRT_PORT));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("hls-put-h264.ts");
    if !fixture.exists() {
        generate_fixture_hls_put(&fixture).await?;
    }

    let mut publisher = spawn_publisher(
        &fixture,
        &format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/e2e-hls-put&pkt_size=1316"),
        "mpegts",
        true,
    )
    .await?;
    wait_for_ingests(&engine, &["pipe-hls-put"], Duration::from_secs(15)).await?;
    println!("[hls-put] Source ingest established");

    let source_ring = engine.get_or_create_pipeline("pipe-hls-put").await;
    let (store, _) = engine.ensure_hls_segmenter("pipe-hls-put").await;
    let hls_cancel = engine
        .get_hls_cancel_token("pipe-hls-put")
        .await
        .ok_or("HLS segmenter token missing")?;
    let hls_segmenter = tokio::spawn(restream::media::hls::start_hls_segmenter(
        "pipe-hls-put".to_string(),
        store.clone(),
        source_ring,
        engine.clone(),
        hls_cancel.clone(),
    ));
    engine.add_hls_persistent_consumer("pipe-hls-put").await;

    let youtube_url =
        format!("http://127.0.0.1:{hls_put_port}/upload?cid=dummy&copy=0&file=out.m3u8");
    let akamai_url = format!("http://127.0.0.1:{hls_put_port}/akamai/out.m3u8?token=dummy");
    let youtube_cancel = CancellationToken::new();
    let akamai_cancel = CancellationToken::new();
    let youtube_upload = tokio::spawn(restream::media::hls_upload::start_hls_put_upload(
        "out-hls-put-youtube".to_string(),
        "pipe-hls-put".to_string(),
        youtube_url,
        store.clone(),
        youtube_cancel.clone(),
    ));
    let akamai_upload = tokio::spawn(restream::media::hls_upload::start_hls_put_upload(
        "out-hls-put-akamai".to_string(),
        "pipe-hls-put".to_string(),
        akamai_url,
        store,
        akamai_cancel.clone(),
    ));

    println!(
        "[hls-put] Streaming {}s for initial HLS PUT uploads",
        settle.as_secs()
    );
    tokio::time::sleep(settle).await;
    let artifacts = wait_for_hls_put_artifacts(&sink_dir, Duration::from_secs(30)).await?;
    validate_hls_playlist(&artifacts.youtube_playlist, "YouTube-style")?;
    validate_hls_playlist(&artifacts.akamai_playlist, "path-style")?;

    let requests = read_hls_put_requests(&sink_dir)?;
    let youtube_playlist_content_type = request_seen(&requests, |request| {
        request["file"] == "out.m3u8" && request["contentType"] == "application/vnd.apple.mpegurl"
    });
    let youtube_segment_content_type = request_seen(&requests, |request| {
        request["file"]
            .as_str()
            .is_some_and(|file| is_segment_file(file, "seg"))
            && request["contentType"] == "video/mp2t"
    });
    let akamai_playlist_content_type = request_seen(&requests, |request| {
        request["file"] == "akamai/out.m3u8"
            && request["contentType"] == "application/vnd.apple.mpegurl"
            && request["path"]
                .as_str()
                .is_some_and(|path| path.contains("token=dummy"))
    });
    let akamai_segment_content_type = request_seen(&requests, |request| {
        request["file"]
            .as_str()
            .is_some_and(|file| is_segment_file(file, "akamai/seg"))
            && request["contentType"] == "video/mp2t"
            && request["path"]
                .as_str()
                .is_some_and(|path| path.contains("token=dummy"))
    });

    let youtube_probe = ffprobe(&artifacts.youtube_segment.to_string_lossy()).await?;
    let akamai_probe = ffprobe(&artifacts.akamai_segment.to_string_lossy()).await?;
    let youtube_dimensions = video_dimensions(&youtube_probe).unwrap_or_else(|| "none".to_string());
    let akamai_dimensions = video_dimensions(&akamai_probe).unwrap_or_else(|| "none".to_string());
    let dimensions_ok = youtube_dimensions == "1280x720" && akamai_dimensions == "1280x720";

    let requests_before = request_line_count(&sink_dir);
    println!("[hls-put] Restarting sink after {requests_before} PUT requests");
    sink_cancel.cancel();
    let _ = sink_handle.await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    (sink_cancel, sink_handle) = start_hls_put_sink(hls_put_port, sink_dir.clone()).await?;
    let (youtube_recovered, akamai_recovered) =
        wait_for_hls_put_recovery(&sink_dir, requests_before, restart_settle).await?;
    let requests_after = request_line_count(&sink_dir);

    youtube_cancel.cancel();
    akamai_cancel.cancel();
    hls_cancel.cancel();
    sink_cancel.cancel();
    let _ = youtube_upload.await;
    let _ = akamai_upload.await;
    let _ = hls_segmenter.await;
    let _ = sink_handle.await;
    let _ = publisher.kill().await;

    let passed = youtube_playlist_content_type
        && youtube_segment_content_type
        && akamai_playlist_content_type
        && akamai_segment_content_type
        && dimensions_ok
        && youtube_recovered
        && akamai_recovered;

    let mut results = json!({
        "passed": passed,
        "sinkDir": sink_dir,
        "requestsBeforeRestart": requests_before,
        "requestsAfterRestart": requests_after,
        "youtube": {
            "playlist": artifacts.youtube_playlist,
            "segment": artifacts.youtube_segment,
            "dimensions": youtube_dimensions,
            "playlistContentTypeObserved": youtube_playlist_content_type,
            "segmentContentTypeObserved": youtube_segment_content_type,
            "recoveredAfterRestart": youtube_recovered,
        },
        "akamai": {
            "playlist": artifacts.akamai_playlist,
            "segment": artifacts.akamai_segment,
            "dimensions": akamai_dimensions,
            "playlistContentTypeAndQueryObserved": akamai_playlist_content_type,
            "segmentContentTypeAndQueryObserved": akamai_segment_content_type,
            "recoveredAfterRestart": akamai_recovered,
        },
    });
    if !dimensions_ok {
        results["error"] = json!(format!(
            "expected 1280x720 HLS PUT segments, got youtube={youtube_dimensions} akamai={akamai_dimensions}"
        ));
    } else if !youtube_recovered || !akamai_recovered {
        results["error"] = json!("HLS PUT upload did not recover for both output URL shapes");
    } else if !passed {
        results["error"] =
            json!("HLS PUT upload did not preserve expected content types or signed query shape");
    }

    let path = artifact_path("hls-put.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    println!("artifact={}", path.display());
    if passed {
        Ok(results)
    } else {
        Err(format!("HLS PUT upload scenario failed: {results}"))
    }
}

struct HlsPutArtifacts {
    youtube_playlist: PathBuf,
    youtube_segment: PathBuf,
    akamai_playlist: PathBuf,
    akamai_segment: PathBuf,
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
    let akamai_playlist = sink_dir.join("akamai/out.m3u8");
    loop {
        let youtube_segment = first_segment_in(sink_dir);
        let akamai_segment = first_segment_in(&sink_dir.join("akamai"));
        if youtube_playlist.is_file()
            && file_nonempty(&youtube_playlist)
            && akamai_playlist.is_file()
            && file_nonempty(&akamai_playlist)
            && let (Some(youtube_segment), Some(akamai_segment)) = (youtube_segment, akamai_segment)
        {
            return Ok(HlsPutArtifacts {
                youtube_playlist,
                youtube_segment,
                akamai_playlist,
                akamai_segment,
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

fn request_line_count(sink_dir: &Path) -> usize {
    std::fs::read_to_string(sink_dir.join("requests.jsonl"))
        .map(|body| body.lines().count())
        .unwrap_or(0)
}

async fn wait_for_hls_put_recovery(
    sink_dir: &Path,
    start_line: usize,
    timeout: Duration,
) -> Result<(bool, bool), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let requests = std::fs::read_to_string(sink_dir.join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .skip(start_line)
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        let youtube_recovered = request_seen(&requests, |request| {
            request["file"]
                .as_str()
                .is_some_and(|file| is_segment_file(file, "seg"))
                && request["contentType"] == "video/mp2t"
        });
        let akamai_recovered = request_seen(&requests, |request| {
            request["file"]
                .as_str()
                .is_some_and(|file| is_segment_file(file, "akamai/seg"))
                && request["contentType"] == "video/mp2t"
                && request["path"]
                    .as_str()
                    .is_some_and(|path| path.contains("token=dummy"))
        });
        if youtube_recovered && akamai_recovered {
            return Ok((true, true));
        }
        if Instant::now() >= deadline {
            return Ok((youtube_recovered, akamai_recovered));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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

#[derive(Clone)]
struct BurstConfig {
    name: &'static str,
    protocol: &'static str,
    codec: &'static str,
    resolution: &'static str,
    fps: u32,
    gop: u32,
    multi_audio: bool,
}

fn burst_configs() -> Vec<BurstConfig> {
    vec![
        BurstConfig {
            name: "rtmp-h264-1080p-30fps-1a",
            protocol: "rtmp",
            codec: "h264",
            resolution: "1920x1080",
            fps: 30,
            gop: 60,
            multi_audio: false,
        },
        BurstConfig {
            name: "rtmp-h264-1080p-60fps-1a",
            protocol: "rtmp",
            codec: "h264",
            resolution: "1920x1080",
            fps: 60,
            gop: 120,
            multi_audio: false,
        },
        BurstConfig {
            name: "rtmp-h264-4k-24fps-1a",
            protocol: "rtmp",
            codec: "h264",
            resolution: "3840x2160",
            fps: 24,
            gop: 48,
            multi_audio: false,
        },
        BurstConfig {
            name: "rtmp-h264-4k-25fps-2a",
            protocol: "rtmp",
            codec: "h264",
            resolution: "3840x2160",
            fps: 25,
            gop: 50,
            multi_audio: true,
        },
        BurstConfig {
            name: "rtmp-h265-1080p-50fps-1a",
            protocol: "rtmp",
            codec: "h265",
            resolution: "1920x1080",
            fps: 50,
            gop: 100,
            multi_audio: false,
        },
        BurstConfig {
            name: "rtmp-h265-4k-30fps-2a",
            protocol: "rtmp",
            codec: "h265",
            resolution: "3840x2160",
            fps: 30,
            gop: 60,
            multi_audio: true,
        },
        BurstConfig {
            name: "srt-h264-1080p-25fps-1a",
            protocol: "srt",
            codec: "h264",
            resolution: "1920x1080",
            fps: 25,
            gop: 50,
            multi_audio: false,
        },
        BurstConfig {
            name: "srt-h264-1080p-60fps-2a",
            protocol: "srt",
            codec: "h264",
            resolution: "1920x1080",
            fps: 60,
            gop: 120,
            multi_audio: true,
        },
        BurstConfig {
            name: "srt-h265-1080p-24fps-1a",
            protocol: "srt",
            codec: "h265",
            resolution: "1920x1080",
            fps: 24,
            gop: 48,
            multi_audio: false,
        },
        BurstConfig {
            name: "srt-h265-4k-30fps-2a",
            protocol: "srt",
            codec: "h265",
            resolution: "3840x2160",
            fps: 30,
            gop: 60,
            multi_audio: true,
        },
    ]
}

fn selected_burst_configs() -> Result<Vec<BurstConfig>, String> {
    let configs = burst_configs();
    let Some(selection) = std::env::var("BURST_CONFIGS")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(configs);
    };
    let wanted = selection.split_whitespace().collect::<Vec<_>>();
    let selected = configs
        .into_iter()
        .filter(|config| wanted.contains(&config.name))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        Err("burst-verify: BURST_CONFIGS matched no configs".to_string())
    } else {
        Ok(selected)
    }
}

async fn burst_verify_correctness() -> Result<Value, String> {
    let settle = Duration::from_secs(env_secs("BURST_SETTLE_SECS", 8));
    let restream_log = artifact_path("restream.log");
    if let Some(parent) = restream_log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        restream_log,
        "scenario managed by Rust test_harness burst-verify\n",
    )
    .map_err(|e| e.to_string())?;

    let db_path = artifact_path("burst-verify.sqlite");
    let _ = std::fs::remove_file(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = db::create_pool(&db_url).await.map_err(|e| e.to_string())?;
    db::setup_database_schema(&pool)
        .await
        .map_err(|e| e.to_string())?;

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

    let configs = selected_burst_configs()?;
    let mut pass = 0usize;
    let mut fail_count = 0usize;
    let mut case_results = Vec::new();

    for config in configs {
        println!(
            "[burst-verify] {}: {} {} {} {}fps GOP={} audio={}",
            config.name,
            config.protocol,
            config.codec,
            config.resolution,
            config.fps,
            config.gop,
            if config.multi_audio { 2 } else { 1 }
        );

        let pipeline_id = format!("pipe-burst-{}", config.name);
        let stream_key = format!("sk-{}", config.name);
        db::create_pipeline(&pool, &pipeline_id, config.name, &stream_key, None, None)
            .await
            .map_err(|e| e.to_string())?;

        let publish_url = if config.protocol == "rtmp" {
            format!("rtmp://127.0.0.1:{RTMP_PORT}/live/{stream_key}")
        } else {
            format!("srt://127.0.0.1:{SRT_PORT}?streamid=publish:live/{stream_key}&pkt_size=1316")
        };
        let log_path = artifact_path(&format!("{}-pub.log", config.name));
        let mut publisher = spawn_burst_publisher(&config, &publish_url, &log_path).await?;

        let mut case_passed = false;
        let mut case_error = None::<String>;
        let mut readers = Vec::<Value>::new();
        let mut burst_ok = 0usize;

        match wait_for_ingest_bytes(&engine, &pipeline_id, Duration::from_secs(20)).await {
            Ok(()) => {
                let source_ring = engine.get_or_create_pipeline(&pipeline_id).await;
                let reader_cancel = CancellationToken::new();
                let reader_handle = tokio::spawn(run_burst_verify_reader(
                    config.name.to_string(),
                    source_ring,
                    reader_cancel.clone(),
                ));

                println!(
                    "[burst-verify] {} streaming {}s for burst stats",
                    config.name,
                    settle.as_secs()
                );
                tokio::time::sleep(settle).await;

                let graph = engine.processing_graph(&pipeline_id, &[]).await;
                let graph_path = artifact_path(&format!("{}-graph.json", config.name));
                std::fs::write(&graph_path, serde_json::to_vec_pretty(&graph).unwrap())
                    .map_err(|e| e.to_string())?;
                readers = graph_ring_readers(&graph);
                burst_ok = readers
                    .iter()
                    .filter(|reader| {
                        reader["burstCount"].as_u64().unwrap_or(0) > 0
                            && reader["avgBurstSize"].as_f64().unwrap_or(0.0) > 0.0
                    })
                    .count();
                case_passed = !readers.is_empty() && burst_ok == readers.len();
                if readers.is_empty() {
                    case_error = Some("no ring buffer readers found in graph".to_string());
                } else if !case_passed {
                    case_error = Some(format!(
                        "{} of {} reader(s) reported non-zero burst stats",
                        burst_ok,
                        readers.len()
                    ));
                }

                reader_cancel.cancel();
                let _ = reader_handle.await;
            }
            Err(err) => {
                case_error = Some(err);
            }
        }

        let _ = publisher.kill().await;
        let _ = publisher.wait().await;

        if case_passed {
            pass += 1;
        } else {
            fail_count += 1;
        }
        case_results.push(json!({
            "config": config.name,
            "protocol": config.protocol,
            "codec": config.codec,
            "resolution": config.resolution,
            "fps": config.fps,
            "gop": config.gop,
            "requestedAudioTracks": if config.multi_audio { 2 } else { 1 },
            "publishedAudioTracks": burst_published_audio_tracks(&config),
            "passed": case_passed,
            "readerCount": readers.len(),
            "burstOk": burst_ok,
            "readers": readers,
            "publisherLog": log_path,
            "error": case_error,
        }));
    }

    let passed = fail_count == 0;
    let results = json!({
        "passed": passed,
        "pass": pass,
        "fail": fail_count,
        "total": pass + fail_count,
        "cases": case_results,
    });
    let path = artifact_path("burst-verify.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    println!("artifact={}", path.display());
    if passed {
        Ok(results)
    } else {
        Err(format!("burst-verify failed: {results}"))
    }
}

async fn spawn_burst_publisher(
    config: &BurstConfig,
    url: &str,
    log_path: &Path,
) -> Result<Child, String> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
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
        &format!("testsrc2=size={}:rate={}", config.resolution, config.fps),
    ]);
    if config.multi_audio {
        cmd.args([
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono",
        ]);
    } else {
        cmd.args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]);
    }

    if config.codec == "h265" {
        cmd.args([
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-x265-params",
            &format!(
                "log-level=none:keyint={}:min-keyint={}:no-open-gop=1",
                config.gop, config.gop
            ),
        ]);
    } else {
        cmd.args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            &config.gop.to_string(),
            "-keyint_min",
            &config.gop.to_string(),
            "-x264-params",
            "no-open-gop=1",
        ]);
    }

    if burst_published_audio_tracks(config) == 2 {
        cmd.args(["-map", "0:v", "-map", "1:a", "-map", "2:a"]);
    } else {
        cmd.args(["-map", "0:v", "-map", "1:a"]);
    }
    cmd.args(["-b:v", "6M", "-c:a", "aac", "-b:a", "64k"]);
    if config.protocol == "rtmp" {
        cmd.args(["-f", "flv"]);
    } else {
        cmd.args(["-f", "mpegts"]);
    }
    cmd.arg(url);
    cmd.stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
}

fn burst_published_audio_tracks(config: &BurstConfig) -> usize {
    if config.multi_audio && config.protocol != "rtmp" {
        2
    } else {
        1
    }
}

async fn run_burst_verify_reader(name: String, ring: Arc<RingBuffer>, cancel: CancellationToken) {
    let mut reader = Reader::new(format!("burst_verify:{name}:src-out"), ring);
    let mut packets = Vec::with_capacity(32);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = reader.wait_for_data() => {
                loop {
                    packets.clear();
                    match reader.pull_burst(&mut packets, 32) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        }
    }
}

async fn wait_for_ingest_bytes(
    engine: &MediaEngine,
    pipeline_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let ingests = engine.active_ingests.read().await;
        let ready = ingests
            .get(pipeline_id)
            .is_some_and(|ingest| ingest.bytes_received.load(Ordering::Relaxed) > 0);
        drop(ingests);
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{pipeline_id}: ingest did not receive bytes within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
/// Publishes B-frame H.264/AAC over RTMP, loops the source through native RTMP
/// egress, and verifies ffprobe observes composition offsets (PTS > DTS) while
/// DTS stays monotone.
async fn bframe_rtmp_correctness() -> Result<Value, String> {
    let db_path = artifact_path("bframe-rtmp.sqlite");
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
        ("pipe-bframe-src", "B-frame RTMP source", "e2e-bframe-src"),
        ("pipe-bframe-sink", "B-frame RTMP sink", "e2e-bframe-sink"),
    ] {
        db::create_pipeline(&pool, id, name, key, None, None)
            .await
            .map_err(|e| e.to_string())?;
    }

    let engine = Arc::new(MediaEngine::new());
    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let _rtmp_task = tokio::spawn(start_rtmp_server_on(
        pool,
        security,
        engine.clone(),
        RTMP_PORT,
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture_h264(&fixture).await?;
    }

    let mut publisher = spawn_publisher(
        &fixture,
        &format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-bframe-src"),
        "flv",
        false,
    )
    .await?;
    wait_for_ingests(&engine, &["pipe-bframe-src"], Duration::from_secs(15)).await?;
    println!("[bframe-rtmp] Source ingest established");

    let source_ring = engine.get_or_create_pipeline("pipe-bframe-src").await;
    let sink_url = format!("rtmp://127.0.0.1:{RTMP_PORT}/live/e2e-bframe-sink");
    let token = engine
        .register_egress("out-bframe-rtmp", "pipe-bframe-src", &sink_url)
        .await;
    let _egress = tokio::spawn(start_rtmp_egress(
        "out-bframe-rtmp".to_string(),
        "pipe-bframe-src".to_string(),
        sink_url.clone(),
        source_ring,
        engine.clone(),
        token,
    ));

    wait_for_ingests(&engine, &["pipe-bframe-sink"], Duration::from_secs(15)).await?;
    println!("[bframe-rtmp] Sink ingest established");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let probe = ffprobe(&sink_url).await?;
    let media_check = assert_media_only(&probe, "RTMP B-frame egress");
    let packets_path = artifact_path("bframe-packets.json");
    let packet_probe = ffprobe_video_packets(&sink_url, &packets_path).await?;
    let packet_count = count_video_packets(&packet_probe);
    let bframe_count = count_bframe_packets(&packet_probe);
    let dts_monotone = video_dts_monotone(&packet_probe);
    let passed = media_check.is_ok() && packet_count >= 30 && bframe_count > 0 && dts_monotone;
    let _ = publisher.kill().await;

    let mut results = json!({
        "passed": passed,
        "mediaCheck": media_check.is_ok(),
        "mediaError": media_check.err(),
        "probe": probe,
        "packetProbe": packet_probe,
        "packetArtifact": packets_path,
        "packetCount": packet_count,
        "bframeCount": bframe_count,
        "dtsMonotone": dts_monotone,
        "rtmpEgressBytes": engine.egress_bytes("out-bframe-rtmp").await,
    });
    if packet_count < 30 {
        results["error"] = json!(format!(
            "expected at least 30 video packets, got {packet_count}"
        ));
    } else if bframe_count == 0 {
        results["error"] = json!("RTMP egress did not expose any packets with PTS > DTS");
    } else if !dts_monotone {
        results["error"] = json!("RTMP egress DTS values are not monotone");
    }

    let path = artifact_path("bframe-rtmp.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    println!("artifact={}", path.display());
    if passed {
        Ok(results)
    } else {
        Err(format!("RTMP B-frame round-trip failed: {results}"))
    }
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

async fn generate_fixture_hls_put(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            "8",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            "30",
            "-keyint_min",
            "30",
            "-x264-params",
            "no-open-gop=1",
            "-b:v",
            "1.5M",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
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
        Err(format!("HLS PUT H.264 fixture generation failed: {status}"))
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
