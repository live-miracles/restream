//! Native SRT ingest and egress via raw `libsrt` FFI bindings.
//!
//! Ingest: SRT listener accepts connections, reads `streamid` for authentication,
//! pipes MPEG-TS data into a `MemoryQueue`, and runs an FFmpeg demuxer on a
//! dedicated OS thread (wrapped in `catch_unwind`). The demuxer publishes ALL
//! video and audio streams (not just "best") into the `RingBuffer` with per-track
//! indices for multi-track audio support.
//!
//! Egress: connects to an SRT target and forwards ring buffer packets.

use bytes::Bytes;
use std::net::SocketAddr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::media::engine::MediaEngine;
use crate::media::engine::{AudioMeta, PublisherQuality, VideoMeta};
use crate::media::ring_buffer::{MediaPacket, MediaType, Reader, RingBuffer};

// Raw SRT Types & FFI Bindings
pub type SRTSOCKET = c_int;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sockaddr_in {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SrtTraceBStats {
    pub ms_time_stamp: i64,
    pub pkt_sent_total: i64,
    pub pkt_recv_total: i64,
    pub pkt_snd_loss_total: c_int,
    pub pkt_rcv_loss_total: c_int,
    pub pkt_retrans_total: c_int,
    pub pkt_sent_ack_total: c_int,
    pub pkt_recv_ack_total: c_int,
    pub pkt_sent_nak_total: c_int,
    pub pkt_recv_nak_total: c_int,
    pub us_snd_duration_total: i64,
    pub pkt_snd_drop_total: c_int,
    pub pkt_rcv_drop_total: c_int,
    pub pkt_rcv_undecrypt_total: c_int,
    pub byte_sent_total: u64,
    pub byte_recv_total: u64,
    pub byte_rcv_loss_total: u64,
    pub byte_retrans_total: u64,
    pub byte_snd_drop_total: u64,
    pub byte_rcv_drop_total: u64,
    pub byte_rcv_undecrypt_total: u64,
    pub pkt_sent: i64,
    pub pkt_recv: i64,
    pub pkt_snd_loss: c_int,
    pub pkt_rcv_loss: c_int,
    pub pkt_retrans: c_int,
    pub pkt_rcv_retrans: c_int,
    pub pkt_sent_ack: c_int,
    pub pkt_recv_ack: c_int,
    pub pkt_sent_nak: c_int,
    pub pkt_recv_nak: c_int,
    pub mbps_send_rate: f64,
    pub mbps_recv_rate: f64,
    pub us_snd_duration: i64,
    pub pkt_reorder_distance: c_int,
    pub pkt_rcv_avg_belated_time: f64,
    pub pkt_rcv_belated: i64,
    pub pkt_snd_drop: c_int,
    pub pkt_rcv_drop: c_int,
    pub pkt_rcv_undecrypt: c_int,
    pub byte_sent: u64,
    pub byte_recv: u64,
    pub byte_rcv_loss: u64,
    pub byte_retrans: u64,
    pub byte_snd_drop: u64,
    pub byte_rcv_drop: u64,
    pub byte_rcv_undecrypt: u64,
    pub us_pkt_snd_period: f64,
    pub pkt_flow_window: c_int,
    pub pkt_congestion_window: c_int,
    pub pkt_flight_size: c_int,
    pub ms_rtt: f64,
    pub mbps_bandwidth: f64,
    pub byte_avail_snd_buf: c_int,
    pub byte_avail_rcv_buf: c_int,
    pub mbps_max_bw: f64,
    pub byte_mss: c_int,
    pub pkt_snd_buf: c_int,
    pub byte_snd_buf: c_int,
    pub ms_snd_buf: c_int,
    pub ms_snd_tsb_pd_delay: c_int,
    pub pkt_rcv_buf: c_int,
    pub byte_rcv_buf: c_int,
    pub ms_rcv_buf: c_int,
    pub ms_rcv_tsb_pd_delay: c_int,
    pub pkt_snd_filter_extra_total: c_int,
    pub pkt_rcv_filter_extra_total: c_int,
    pub pkt_rcv_filter_supply_total: c_int,
    pub pkt_rcv_filter_loss_total: c_int,
    pub pkt_snd_filter_extra: c_int,
    pub pkt_rcv_filter_extra: c_int,
    pub pkt_rcv_filter_supply: c_int,
    pub pkt_rcv_filter_loss: c_int,
    pub pkt_reorder_tolerance: c_int,
    pub pkt_sent_unique_total: i64,
    pub pkt_recv_unique_total: i64,
    pub byte_sent_unique_total: u64,
    pub byte_recv_unique_total: u64,
    pub pkt_sent_unique: i64,
    pub pkt_recv_unique: i64,
    pub byte_sent_unique: u64,
    pub byte_recv_unique: u64,
}

unsafe extern "C" {
    pub fn srt_startup() -> c_int;
    pub fn srt_cleanup() -> c_int;
    pub fn srt_create_socket() -> SRTSOCKET;
    pub fn srt_create_group(gtype: c_int) -> SRTSOCKET;
    pub fn srt_close(u: SRTSOCKET) -> c_int;
    pub fn srt_bind(u: SRTSOCKET, name: *const sockaddr_in, namelen: c_int) -> c_int;
    pub fn srt_listen(u: SRTSOCKET, backlog: c_int) -> c_int;
    pub fn srt_accept(u: SRTSOCKET, addr: *mut sockaddr_in, addrlen: *mut c_int) -> SRTSOCKET;
    pub fn srt_connect(u: SRTSOCKET, name: *const sockaddr_in, namelen: c_int) -> c_int;
    pub fn srt_recv(u: SRTSOCKET, buf: *mut u8, len: c_int) -> c_int;
    pub fn srt_send(u: SRTSOCKET, buf: *const u8, len: c_int) -> c_int;
    pub fn srt_setsockopt(
        u: SRTSOCKET,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: c_int,
    ) -> c_int;
    pub fn srt_getsockopt(
        u: SRTSOCKET,
        level: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut c_int,
    ) -> c_int;
    pub fn srt_getlasterror_str() -> *const c_char;
    pub fn srt_bistats(
        u: SRTSOCKET,
        perf: *mut SrtTraceBStats,
        clear: c_int,
        instantaneous: c_int,
    ) -> c_int;
}

// SRT socket options constants
pub const SRTO_SNDSYN: c_int = 1;
pub const SRTO_RCVSYN: c_int = 2;
pub const SRTO_STREAMID: c_int = 46;
pub const SRTO_TRANSTYPE: c_int = 50;
pub const SRTO_GROUPCONNECT: c_int = 51;

pub const SRTT_LIVE: c_int = 0;

fn to_sockaddr_in(addr: SocketAddr) -> sockaddr_in {
    let ip = match addr.ip() {
        std::net::IpAddr::V4(ipv4) => u32::from_ne_bytes(ipv4.octets()),
        _ => 0,
    };
    sockaddr_in {
        sin_family: 2, // AF_INET
        sin_port: addr.port().to_be(),
        sin_addr: ip,
        sin_zero: [0; 8],
    }
}

fn from_sockaddr_in(addr: sockaddr_in) -> SocketAddr {
    SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(addr.sin_addr.to_ne_bytes())),
        u16::from_be(addr.sin_port),
    )
}

#[derive(Debug, Clone, Copy)]
struct SrtCounterSnapshot {
    packets_received_loss: u64,
    packets_received_drop: u64,
    packets_received_retrans: u64,
    packets_received_undecrypt: u64,
    sampled_at: Instant,
}

fn counter_rate(current: u64, previous: u64, elapsed_seconds: f64) -> Option<f64> {
    if elapsed_seconds <= 0.0 {
        return None;
    }
    current
        .checked_sub(previous)
        .map(|delta| (delta as f64 / elapsed_seconds * 10.0).round() / 10.0)
}

fn srt_quality_from_stats(
    stats: &SrtTraceBStats,
    previous: Option<SrtCounterSnapshot>,
    sampled_at: Instant,
) -> (PublisherQuality, SrtCounterSnapshot) {
    let current = SrtCounterSnapshot {
        packets_received_loss: stats.pkt_rcv_loss_total.max(0) as u64,
        packets_received_drop: stats.pkt_rcv_drop_total.max(0) as u64,
        packets_received_retrans: stats.pkt_rcv_retrans.max(0) as u64,
        packets_received_undecrypt: stats.pkt_rcv_undecrypt_total.max(0) as u64,
        sampled_at,
    };
    let elapsed =
        previous.map(|snapshot| sampled_at.duration_since(snapshot.sampled_at).as_secs_f64());

    (
        PublisherQuality {
            ms_rtt: Some(stats.ms_rtt),
            mbps_receive_rate: Some(stats.mbps_recv_rate),
            mbps_link_capacity: Some(stats.mbps_bandwidth),
            ms_receive_tsb_pd_delay: Some(stats.ms_rcv_tsb_pd_delay.max(0) as f64),
            ms_receive_buf: Some(stats.ms_rcv_buf.max(0) as f64),
            packets_sent_nak: Some(stats.pkt_sent_nak_total.max(0) as u64),
            packets_received_loss: Some(current.packets_received_loss),
            packets_received_drop: Some(current.packets_received_drop),
            packets_received_retrans: Some(current.packets_received_retrans),
            packets_received_undecrypt: Some(current.packets_received_undecrypt),
            packets_received_loss_per_sec: previous.zip(elapsed).and_then(|(snapshot, seconds)| {
                counter_rate(
                    current.packets_received_loss,
                    snapshot.packets_received_loss,
                    seconds,
                )
            }),
            packets_received_drop_per_sec: previous.zip(elapsed).and_then(|(snapshot, seconds)| {
                counter_rate(
                    current.packets_received_drop,
                    snapshot.packets_received_drop,
                    seconds,
                )
            }),
            packets_received_retrans_per_sec: previous.zip(elapsed).and_then(
                |(snapshot, seconds)| {
                    counter_rate(
                        current.packets_received_retrans,
                        snapshot.packets_received_retrans,
                        seconds,
                    )
                },
            ),
            packets_received_undecrypt_per_sec: previous.zip(elapsed).and_then(
                |(snapshot, seconds)| {
                    counter_rate(
                        current.packets_received_undecrypt,
                        snapshot.packets_received_undecrypt,
                        seconds,
                    )
                },
            ),
            ..PublisherQuality::default()
        },
        current,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SrtConnectionMode {
    Publish,
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedStreamId {
    mode: SrtConnectionMode,
    stream_key: String,
}

fn strip_query(value: &str) -> &str {
    value.split_once('?').map(|(path, _)| path).unwrap_or(value)
}

fn parse_srt_stream_id(streamid: &str) -> ParsedStreamId {
    let raw = streamid.trim_matches('\0').trim();
    if raw.is_empty() {
        return ParsedStreamId {
            mode: SrtConnectionMode::Publish,
            stream_key: String::new(),
        };
    }

    if let Some(rest) = raw.strip_prefix("#!::") {
        let mut mode = SrtConnectionMode::Publish;
        let mut resource = "";
        for part in rest.split(',') {
            if let Some((key, value)) = part.split_once('=') {
                match key {
                    "r" | "streamid" => resource = value,
                    "m" => {
                        if matches!(value, "request" | "read" | "play" | "subscriber") {
                            mode = SrtConnectionMode::Read;
                        }
                    }
                    _ => {}
                }
            }
        }
        let stream_key = strip_query(resource)
            .rsplit('/')
            .next()
            .unwrap_or(resource)
            .to_string();
        return ParsedStreamId { mode, stream_key };
    }

    let (mode, rest) = if let Some((prefix, value)) = raw.split_once(':') {
        let mode = if matches!(prefix, "play" | "read" | "subscriber" | "request") {
            SrtConnectionMode::Read
        } else {
            SrtConnectionMode::Publish
        };
        (mode, value)
    } else {
        (SrtConnectionMode::Publish, raw)
    };

    let stream_key = strip_query(rest)
        .rsplit('/')
        .next()
        .unwrap_or(rest)
        .to_string();
    ParsedStreamId { mode, stream_key }
}

fn video_codec_id(codec: &str) -> Option<ffmpeg_next::ffi::AVCodecID> {
    match codec {
        "h264" | "avc" => Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_H264),
        "h265" | "hevc" => Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC),
        _ => None,
    }
}

fn audio_codec_id(codec: &str) -> Option<ffmpeg_next::ffi::AVCodecID> {
    match codec {
        "aac" => Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_AAC),
        _ => None,
    }
}

pub struct SrtServer {
    db: sqlx::SqlitePool,
    engine: Arc<MediaEngine>,
}

impl SrtServer {
    pub fn new(db: sqlx::SqlitePool, engine: Arc<MediaEngine>) -> Self {
        unsafe {
            srt_startup();
        }
        Self { db, engine }
    }

    pub async fn run(self: Arc<Self>, port: u16) {
        let server_sock = unsafe { srt_create_socket() };
        if server_sock < 0 {
            eprintln!("[srt] Failed to create socket");
            return;
        }

        unsafe {
            let live_mode: c_int = SRTT_LIVE;
            srt_setsockopt(
                server_sock,
                0,
                SRTO_TRANSTYPE,
                &live_mode as *const _ as *const c_void,
                std::mem::size_of::<c_int>() as c_int,
            );
        }

        let addr_str = format!("0.0.0.0:{}", port);
        let addr = match addr_str.parse::<SocketAddr>() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[srt] Invalid address: {:?}", e);
                return;
            }
        };

        let sin = to_sockaddr_in(addr);
        let bind_res = unsafe {
            srt_bind(
                server_sock,
                &sin,
                std::mem::size_of::<sockaddr_in>() as c_int,
            )
        };
        if bind_res < 0 {
            eprintln!("[srt] Bind failed");
            unsafe {
                srt_close(server_sock);
            }
            return;
        }

        let listen_res = unsafe { srt_listen(server_sock, 1024) };
        if listen_res < 0 {
            eprintln!("[srt] Listen failed");
            unsafe {
                srt_close(server_sock);
            }
            return;
        }

        println!("[srt] Server listening on srt://{}", addr_str);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(SRTSOCKET, sockaddr_in)>();

        // Blocking accept thread — srt_accept in sync mode blocks until a connection arrives
        std::thread::spawn(move || {
            loop {
                let mut client_sin = sockaddr_in {
                    sin_family: 0,
                    sin_port: 0,
                    sin_addr: 0,
                    sin_zero: [0; 8],
                };
                let mut len = std::mem::size_of::<sockaddr_in>() as c_int;
                let client_sock = unsafe { srt_accept(server_sock, &mut client_sin, &mut len) };
                if client_sock < 0 {
                    let err = unsafe { std::ffi::CStr::from_ptr(srt_getlasterror_str()) };
                    eprintln!("[srt] Accept error: {}", err.to_string_lossy());
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                if tx.send((client_sock, client_sin)).is_err() {
                    unsafe {
                        srt_close(client_sock);
                    }
                    break;
                }
            }
        });

        while let Some((client_sock, client_addr)) = rx.recv().await {
            let self_clone = self.clone();
            tokio::spawn(async move {
                self_clone
                    .handle_client(client_sock, from_sockaddr_in(client_addr))
                    .await;
            });
        }
    }

    async fn handle_client(&self, client_sock: SRTSOCKET, client_addr: SocketAddr) {
        // Read streamid
        let mut streamid_buf = [0u8; 512];
        let mut optlen = streamid_buf.len() as c_int;
        let res = unsafe {
            srt_getsockopt(
                client_sock,
                0,
                SRTO_STREAMID,
                streamid_buf.as_mut_ptr() as *mut c_void,
                &mut optlen,
            )
        };

        let streamid = if res >= 0 {
            String::from_utf8_lossy(&streamid_buf[..optlen as usize])
                .trim_matches('\0')
                .to_string()
        } else {
            "".to_string()
        };

        println!("[srt] Connection accepted. StreamID: {}", streamid);

        let parsed = parse_srt_stream_id(&streamid);
        let is_reader = parsed.mode == SrtConnectionMode::Read;
        let stream_key = parsed.stream_key.as_str();

        // Query pipeline for stream key validation
        let pipeline = match sqlx::query_as::<_, crate::types::Pipeline>(
            "SELECT id, name, stream_key, input_source, encoding FROM pipelines WHERE stream_key = ?"
        )
        .bind(stream_key)
        .fetch_optional(&self.db)
        .await {
            Ok(Some(p)) => p,
            _ => {
                eprintln!("[srt] Unauthorized connection for stream key: {}", stream_key);
                unsafe { srt_close(client_sock); }
                return;
            }
        };

        println!(
            "[srt] Authenticated stream key: {} for pipeline: {} (mode={})",
            stream_key,
            pipeline.id,
            if is_reader { "read" } else { "publish" }
        );

        if is_reader {
            self.handle_play(client_sock, &pipeline.id).await;
            return;
        }

        let ring_buffer = self.engine.get_or_create_pipeline(&pipeline.id).await;
        let token = self
            .engine
            .register_ingest(&pipeline.id, stream_key, "srt")
            .await;
        self.engine
            .update_ingest_meta(&pipeline.id, None, None, Some(client_addr.to_string()))
            .await;

        // In-memory queue instead of TCP loopback
        let queue = Arc::new(crate::media::avio::MemoryQueue::new());

        // Spawn thread to run FFmpeg demuxer on the custom AVIO context
        let queue_clone = queue.clone();
        let ring_buffer_clone = ring_buffer.clone();
        let token_clone = token.clone();
        let (probe_tx, probe_rx) = std::sync::mpsc::channel::<DemuxProbe>();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_ffmpeg_demuxer(queue_clone, ring_buffer_clone, token_clone, probe_tx)
            }));
            match result {
                Ok(Err(e)) => eprintln!("[srt] FFmpeg demuxer failed: {:?}", e),
                Err(_) => eprintln!("[srt] FFmpeg demuxer panicked"),
                _ => {}
            }
        });

        // Receive probe metadata from demuxer thread (non-blocking check each recv loop)
        let engine_probe = self.engine.clone();
        let pid_probe = pipeline.id.clone();
        tokio::spawn(async move {
            if let Ok(probe) = probe_rx.recv() {
                let first_audio = probe.audio_tracks.first().cloned();
                engine_probe
                    .update_ingest_meta(&pid_probe, probe.video, first_audio, None)
                    .await;
                if !probe.audio_tracks.is_empty() {
                    engine_probe
                        .update_ingest_audio_tracks(&pid_probe, probe.audio_tracks)
                        .await;
                }
            }
        });

        let mut buf = vec![0u8; 1316]; // SRT packet size
        let mut previous_stats: Option<SrtCounterSnapshot> = None;
        let mut last_stats_sample = Instant::now() - std::time::Duration::from_secs(1);
        loop {
            if token.is_cancelled() {
                break;
            }

            let n = unsafe { srt_recv(client_sock, buf.as_mut_ptr(), buf.len() as c_int) };
            if n <= 0 {
                break;
            }

            queue.write(&buf[..n as usize]);

            // Update stats
            self.engine
                .update_ingest_bytes(&pipeline.id, n as u64)
                .await;

            if last_stats_sample.elapsed() >= std::time::Duration::from_secs(1) {
                let mut stats: SrtTraceBStats = unsafe { std::mem::zeroed() };
                if unsafe { srt_bistats(client_sock, &mut stats, 0, 1) } >= 0 {
                    let sampled_at = Instant::now();
                    let (quality, snapshot) =
                        srt_quality_from_stats(&stats, previous_stats, sampled_at);
                    previous_stats = Some(snapshot);
                    last_stats_sample = sampled_at;
                    self.engine
                        .update_publisher_quality(&pipeline.id, quality)
                        .await;
                }
            }
        }

        queue.close();

        println!("[srt] Ingest stream finished for pipeline: {}", pipeline.id);
        self.engine.unregister_ingest(&pipeline.id).await;
        unsafe {
            srt_close(client_sock);
        }
    }

    async fn handle_play(&self, client_sock: SRTSOCKET, pipeline_id: &str) {
        // Verify active ingest exists
        if !self
            .engine
            .active_ingests
            .read()
            .await
            .contains_key(pipeline_id)
        {
            eprintln!("[srt] No active ingest for play: {}", pipeline_id);
            unsafe {
                srt_close(client_sock);
            }
            return;
        }

        let ring_buf = self.engine.get_or_create_pipeline(pipeline_id).await;
        let mut reader = Reader::new(ring_buf);

        // Use MemoryQueue + FFmpeg to mux MediaPackets back to MPEG-TS
        let out_queue = Arc::new(crate::media::avio::MemoryQueue::new());
        let out_queue_reader = out_queue.clone();

        let (video_meta, audio_tracks, flv_payloads) = {
            let ingests = self.engine.active_ingests.read().await;
            match ingests.get(pipeline_id) {
                Some(i) => {
                    let mut audio_tracks = i.audio_tracks.lock().unwrap().clone();
                    if audio_tracks.is_empty() {
                        if let Some(audio) = i.audio.clone() {
                            audio_tracks.push(audio);
                        }
                    }
                    (
                        i.video.clone(),
                        audio_tracks,
                        matches!(i.protocol.as_str(), "rtmp" | "file"),
                    )
                }
                None => {
                    eprintln!("[srt] No active ingest for play: {}", pipeline_id);
                    unsafe {
                        srt_close(client_sock);
                    }
                    return;
                }
            }
        };
        let (video_sh, audio_sh) = self.engine.get_sequence_headers(pipeline_id).await;

        // Muxer thread: reads packets from channel, writes MPEG-TS to out_queue
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<Arc<MediaPacket>>(256);
        let out_queue_mux = out_queue.clone();

        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_play_muxer(
                    out_queue_mux,
                    pkt_rx,
                    video_meta,
                    audio_tracks,
                    flv_payloads,
                    video_sh,
                    audio_sh,
                )
            }));
            match result {
                Ok(Err(e)) => eprintln!("[srt] Play muxer failed: {}", e),
                Err(_) => eprintln!("[srt] Play muxer panicked"),
                _ => {}
            }
        });

        // Sender thread: reads MPEG-TS from out_queue, sends via SRT
        let out_queue_send = out_queue_reader;
        let pid_log = pipeline_id.to_string();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1316];
            loop {
                let n = out_queue_send.read(&mut buf);
                if n == 0 {
                    break;
                }
                let sent = unsafe { srt_send(client_sock, buf.as_ptr(), n as c_int) };
                if sent < 0 {
                    break;
                }
            }
            println!(
                "[srt] Play subscriber disconnected for pipeline: {}",
                pid_log
            );
            unsafe {
                srt_close(client_sock);
            }
        });

        // Feed loop: read from RingBuffer, send to muxer
        loop {
            match reader.pull() {
                Ok(Some(pkt)) => {
                    if pkt_tx.send(pkt).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    // Check if ingest is still active
                    if !self
                        .engine
                        .active_ingests
                        .read()
                        .await
                        .contains_key(pipeline_id)
                    {
                        break;
                    }
                    reader.wait_for_data().await;
                }
                Err(_) => {
                    // Overflow — reader was fast-forwarded
                }
            }
        }

        drop(pkt_tx);
        out_queue.close();
    }
}

fn run_play_muxer(
    out_queue: Arc<crate::media::avio::MemoryQueue>,
    pkt_rx: std::sync::mpsc::Receiver<Arc<MediaPacket>>,
    video_meta: Option<VideoMeta>,
    audio_tracks: Vec<AudioMeta>,
    flv_payloads: bool,
    video_seq_header: Option<Bytes>,
    audio_seq_header: Option<Bytes>,
) -> Result<(), String> {
    use crate::media::avio::CustomOutput;

    let mut custom_output = CustomOutput::new(&*out_queue, "mpegts").map_err(|e| e.to_string())?;
    let octx = custom_output
        .output
        .as_mut()
        .ok_or("Failed to get output context")?;

    let mut video_stream_idx = None;
    let mut audio_stream_indices = Vec::new();

    if let Some(video) = &video_meta {
        let codec_id = video_codec_id(&video.codec)
            .ok_or_else(|| format!("Unsupported video codec for MPEG-TS: {}", video.codec))?;
        let mut stream = octx
            .add_stream(None)
            .map_err(|_| "Failed to add video stream")?;
        unsafe {
            let par = (*stream.as_mut_ptr()).codecpar;
            (*par).codec_type = ffmpeg_next::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*par).codec_id = codec_id;
            (*par).width = video.width as i32;
            (*par).height = video.height as i32;
            (*stream.as_mut_ptr()).time_base = ffmpeg_next::ffi::AVRational { num: 1, den: 1000 };
            // Set extradata from FLV video sequence header (AVCDecoderConfigurationRecord)
            if let Some(ref vsh) = video_seq_header {
                if vsh.len() > 5 {
                    let extradata = &vsh[5..]; // skip FLV header (frame_type|codec + avc_type + 3-byte CTS)
                    let buf = ffmpeg_next::ffi::av_malloc(extradata.len()) as *mut u8;
                    if !buf.is_null() {
                        std::ptr::copy_nonoverlapping(extradata.as_ptr(), buf, extradata.len());
                        (*par).extradata = buf;
                        (*par).extradata_size = extradata.len() as c_int;
                    }
                }
            }
        }
        video_stream_idx = Some(stream.index());
    }

    for audio in &audio_tracks {
        let codec_id = audio_codec_id(&audio.codec)
            .ok_or_else(|| format!("Unsupported audio codec for MPEG-TS: {}", audio.codec))?;
        let mut stream = octx
            .add_stream(None)
            .map_err(|_| "Failed to add audio stream")?;
        unsafe {
            let par = (*stream.as_mut_ptr()).codecpar;
            (*par).codec_type = ffmpeg_next::ffi::AVMediaType::AVMEDIA_TYPE_AUDIO;
            (*par).codec_id = codec_id;
            (*par).sample_rate = audio.sample_rate as i32;
            (*par).ch_layout.nb_channels = audio.channels as i32;
            (*stream.as_mut_ptr()).time_base = ffmpeg_next::ffi::AVRational { num: 1, den: 1000 };
            // Set extradata from FLV audio sequence header (AudioSpecificConfig)
            if audio.track_index == 0 {
                if let Some(ref ash) = audio_seq_header {
                    if ash.len() > 2 {
                        let extradata = &ash[2..]; // skip FLV header (sound_format|etc + aac_type)
                        let buf = ffmpeg_next::ffi::av_malloc(extradata.len()) as *mut u8;
                        if !buf.is_null() {
                            std::ptr::copy_nonoverlapping(extradata.as_ptr(), buf, extradata.len());
                            (*par).extradata = buf;
                            (*par).extradata_size = extradata.len() as c_int;
                        }
                    }
                }
            }
        }
        audio_stream_indices.push((audio.track_index, stream.index()));
    }

    octx.write_header()
        .map_err(|e| format!("Write header: {}", e))?;

    while let Ok(pkt) = pkt_rx.recv() {
        let (stream_idx, payload) = match pkt.media_type {
            MediaType::Video => match video_stream_idx {
                Some(i) => match video_payload_for_mux(&pkt.payload, flv_payloads) {
                    Some(payload) => (i, payload),
                    None => continue,
                },
                None => continue,
            },
            MediaType::Audio => match audio_stream_indices
                .iter()
                .find(|(track_index, _)| *track_index == pkt.track_index)
                .map(|(_, stream_index)| *stream_index)
            {
                Some(i) => match audio_payload_for_mux(&pkt.payload, flv_payloads) {
                    Some(payload) => (i, payload),
                    None => continue,
                },
                None => continue,
            },
        };

        let mut av_pkt = ffmpeg_next::Packet::copy(payload);
        av_pkt.set_stream(stream_idx);
        av_pkt.set_pts(Some(pkt.pts));
        av_pkt.set_dts(Some(pkt.dts));
        if pkt.is_keyframe {
            av_pkt.set_flags(ffmpeg_next::codec::packet::flag::Flags::KEY);
        }

        let _ = av_pkt.write_interleaved(octx);
    }

    octx.write_trailer()
        .map_err(|e| format!("Write trailer: {}", e))?;
    Ok(())
}

fn video_payload_for_mux(payload: &[u8], flv_payloads: bool) -> Option<&[u8]> {
    if !flv_payloads {
        return (!payload.is_empty()).then_some(payload);
    }
    if payload.len() <= 5 {
        return None;
    }
    // FLV/RTMP video payload: [frame_type|codec][packet_type][composition_time:3].
    // Packet type 0 is sequence header and belongs in codec extradata, not media.
    if payload[1] == 0 {
        return None;
    }
    Some(&payload[5..])
}

fn audio_payload_for_mux(payload: &[u8], flv_payloads: bool) -> Option<&[u8]> {
    if !flv_payloads {
        return (!payload.is_empty()).then_some(payload);
    }
    if payload.len() <= 2 {
        return None;
    }
    // FLV/RTMP AAC payload: [sound_format|rate|size|type][aac_packet_type].
    // AAC packet type 0 is AudioSpecificConfig and belongs in extradata.
    if payload[1] == 0 {
        return None;
    }
    Some(&payload[2..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_srt_stream_ids_from_common_tools() {
        let cases = [
            (
                "publish:live/key01?latency=240000",
                SrtConnectionMode::Publish,
                "key01",
            ),
            ("publisher:key02", SrtConnectionMode::Publish, "key02"),
            ("key03", SrtConnectionMode::Publish, "key03"),
            ("read:live/key04", SrtConnectionMode::Read, "key04"),
            ("play:key05", SrtConnectionMode::Read, "key05"),
            ("subscriber:live/key06", SrtConnectionMode::Read, "key06"),
            (
                "#!::r=live/key07,m=publish,latency=240000",
                SrtConnectionMode::Publish,
                "key07",
            ),
            (
                "#!::r=live/key08,m=request",
                SrtConnectionMode::Read,
                "key08",
            ),
        ];

        for (input, mode, key) in cases {
            let parsed = parse_srt_stream_id(input);
            assert_eq!(parsed.mode, mode, "input={}", input);
            assert_eq!(parsed.stream_key, key, "input={}", input);
        }
    }

    #[test]
    fn srt_rates_use_counter_deltas_instead_of_cumulative_totals() {
        let sampled_at = Instant::now();
        let mut stats: SrtTraceBStats = unsafe { std::mem::zeroed() };
        stats.pkt_rcv_loss_total = 5_000;
        stats.pkt_rcv_drop_total = 500;
        stats.pkt_rcv_retrans = 10_000;

        let (first, snapshot) = srt_quality_from_stats(&stats, None, sampled_at);
        assert_eq!(first.packets_received_loss, Some(5_000));
        assert_eq!(first.packets_received_loss_per_sec, None);

        let (recovered, _) = srt_quality_from_stats(
            &stats,
            Some(snapshot),
            sampled_at + std::time::Duration::from_secs(2),
        );
        assert_eq!(recovered.packets_received_loss_per_sec, Some(0.0));
        assert_eq!(recovered.packets_received_drop_per_sec, Some(0.0));
        assert_eq!(recovered.packets_received_retrans_per_sec, Some(0.0));
    }

    #[test]
    fn srt_rates_report_current_loss_window() {
        let sampled_at = Instant::now();
        let previous = SrtCounterSnapshot {
            packets_received_loss: 100,
            packets_received_drop: 10,
            packets_received_retrans: 200,
            packets_received_undecrypt: 0,
            sampled_at,
        };
        let mut stats: SrtTraceBStats = unsafe { std::mem::zeroed() };
        stats.pkt_rcv_loss_total = 120;
        stats.pkt_rcv_drop_total = 16;
        stats.pkt_rcv_retrans = 220;
        stats.pkt_rcv_undecrypt_total = 2;

        let (quality, _) = srt_quality_from_stats(
            &stats,
            Some(previous),
            sampled_at + std::time::Duration::from_secs(2),
        );
        assert_eq!(quality.packets_received_loss_per_sec, Some(10.0));
        assert_eq!(quality.packets_received_drop_per_sec, Some(3.0));
        assert_eq!(quality.packets_received_retrans_per_sec, Some(10.0));
        assert_eq!(quality.packets_received_undecrypt_per_sec, Some(1.0));
    }

    #[test]
    fn mux_payload_extraction_preserves_demuxed_srt_packets() {
        let raw_video = [0, 0, 1, 0x65, 0xaa, 0xbb];
        let raw_audio = [0x21, 0x10, 0x56, 0xe5];

        assert_eq!(
            video_payload_for_mux(&raw_video, false),
            Some(raw_video.as_slice())
        );
        assert_eq!(
            audio_payload_for_mux(&raw_audio, false),
            Some(raw_audio.as_slice())
        );
    }

    #[test]
    fn mux_payload_extraction_strips_flv_wrappers_and_skips_sequence_headers() {
        let flv_video_seq = [0x17, 0x00, 0x00, 0x00, 0x00, 1, 2, 3];
        let flv_video_frame = [0x27, 0x01, 0x00, 0x00, 0x00, 4, 5, 6];
        let flv_audio_seq = [0xaf, 0x00, 0x12, 0x10];
        let flv_audio_frame = [0xaf, 0x01, 0x21, 0x10];

        assert_eq!(video_payload_for_mux(&flv_video_seq, true), None);
        assert_eq!(
            video_payload_for_mux(&flv_video_frame, true),
            Some(&flv_video_frame[5..])
        );
        assert_eq!(audio_payload_for_mux(&flv_audio_seq, true), None);
        assert_eq!(
            audio_payload_for_mux(&flv_audio_frame, true),
            Some(&flv_audio_frame[2..])
        );
    }

    #[test]
    fn maps_h264_and_h265_without_guessing_unknown_codecs() {
        assert_eq!(
            video_codec_id("h264"),
            Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_H264)
        );
        assert_eq!(
            video_codec_id("hevc"),
            Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC)
        );
        assert_eq!(video_codec_id("unknown"), None);
        assert_eq!(
            audio_codec_id("aac"),
            Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_AAC)
        );
        assert_eq!(audio_codec_id("opus"), None);
    }
}

impl Drop for SrtServer {
    fn drop(&mut self) {
        unsafe {
            srt_cleanup();
        }
    }
}

/// Probe result communicated from the demuxer thread back to the async context.
struct DemuxProbe {
    video: Option<VideoMeta>,
    audio_tracks: Vec<AudioMeta>,
}

fn run_ffmpeg_demuxer(
    queue: Arc<crate::media::avio::MemoryQueue>,
    ring_buf: Arc<RingBuffer>,
    token: CancellationToken,
    probe_tx: std::sync::mpsc::Sender<DemuxProbe>,
) -> Result<(), &'static str> {
    use crate::media::avio::CustomInput;

    let mut custom_input = CustomInput::new(&*queue)?;
    let mut ictx = custom_input
        .input
        .take()
        .ok_or("Failed to get CustomInput context")?;

    // Extract stream metadata before reading packets
    let mut video_meta = None;
    let mut audio_metas = Vec::new();

    let mut stream_map: Vec<(Option<MediaType>, u32)> = Vec::new();
    let mut audio_track = 0u32;
    for s in ictx.streams() {
        let params = s.parameters();
        match params.medium() {
            ffmpeg_next::media::Type::Video => {
                if video_meta.is_none() {
                    let codec_id = unsafe { (*params.as_ptr()).codec_id };
                    let codec_name = unsafe {
                        let desc = ffmpeg_next::ffi::avcodec_descriptor_get(codec_id);
                        if desc.is_null() {
                            "unknown".to_string()
                        } else {
                            std::ffi::CStr::from_ptr((*desc).name)
                                .to_string_lossy()
                                .to_string()
                        }
                    };
                    let width = unsafe { (*params.as_ptr()).width } as u32;
                    let height = unsafe { (*params.as_ptr()).height } as u32;
                    let r_fr = s.rate();
                    let fps = if r_fr.1 > 0 {
                        r_fr.0 as f64 / r_fr.1 as f64
                    } else {
                        0.0
                    };
                    let profile = unsafe {
                        let p = (*params.as_ptr()).profile;
                        if p >= 0 {
                            let name = ffmpeg_next::ffi::avcodec_profile_name(codec_id, p);
                            if !name.is_null() {
                                Some(std::ffi::CStr::from_ptr(name).to_string_lossy().to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    let level = unsafe {
                        let l = (*params.as_ptr()).level;
                        if l >= 0 {
                            Some(format!("{}.{}", l / 10, l % 10))
                        } else {
                            None
                        }
                    };
                    video_meta = Some(VideoMeta {
                        codec: codec_name,
                        width,
                        height,
                        fps,
                        bw: None,
                        profile,
                        level,
                        pixel_format: None,
                    });
                    stream_map.push((Some(MediaType::Video), 0));
                } else {
                    // The current pipeline contract carries one video program.
                    // Never merge packets from a second video PID into it.
                    stream_map.push((None, 0));
                }
            }
            ffmpeg_next::media::Type::Audio => {
                let codec_id = unsafe { (*params.as_ptr()).codec_id };
                let codec_name = unsafe {
                    let desc = ffmpeg_next::ffi::avcodec_descriptor_get(codec_id);
                    if desc.is_null() {
                        "unknown".to_string()
                    } else {
                        std::ffi::CStr::from_ptr((*desc).name)
                            .to_string_lossy()
                            .to_string()
                    }
                };
                let sample_rate = unsafe { (*params.as_ptr()).sample_rate } as u32;
                let ch = unsafe { (*params.as_ptr()).ch_layout.nb_channels } as u32;
                audio_metas.push(AudioMeta {
                    codec: codec_name,
                    sample_rate,
                    channels: ch,
                    channel_layout: None,
                    track_index: audio_track,
                });
                stream_map.push((Some(MediaType::Audio), audio_track));
                audio_track += 1;
            }
            _ => stream_map.push((None, 0)),
        }
    }

    // Report probed metadata
    if video_meta.is_some() || !audio_metas.is_empty() {
        if let Some(ref v) = video_meta {
            println!(
                "[srt] Probed video: {} {}x{} {:.1}fps profile={:?}",
                v.codec, v.width, v.height, v.fps, v.profile
            );
        }
        for a in &audio_metas {
            println!(
                "[srt] Probed audio track {}: {} {}Hz {}ch",
                a.track_index, a.codec, a.sample_rate, a.channels
            );
        }
        let _ = probe_tx.send(DemuxProbe {
            video: video_meta,
            audio_tracks: audio_metas,
        });
    }
    drop(probe_tx);

    for (stream, packet) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }

        let (media_type, track_index) =
            stream_map.get(stream.index()).copied().unwrap_or((None, 0));

        if let Some(mt) = media_type {
            let pts = packet.pts().unwrap_or(0);
            let dts = packet.dts().unwrap_or(0);

            let time_base = stream.time_base();
            let pts_ms = (pts as f64 * time_base.0 as f64 / time_base.1 as f64 * 1000.0) as i64;
            let dts_ms = (dts as f64 * time_base.0 as f64 / time_base.1 as f64 * 1000.0) as i64;

            let media_pkt = MediaPacket {
                media_type: mt,
                track_index,
                pts: pts_ms,
                dts: dts_ms,
                is_keyframe: packet.is_key(),
                payload: Bytes::copy_from_slice(packet.data().unwrap_or(&[])),
            };

            ring_buf.push(media_pkt);
        }
    }

    Ok(())
}

// SRT Egress Client
pub async fn start_srt_egress(
    output_id: String,
    target_url: String,
    ring_buffer: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel_token: CancellationToken,
) {
    // Parse target_url: e.g. "srt://host:port?streamid=..."
    // Extract host and port
    let url_cleaned = target_url.replace("srt://", "");
    let parts: Vec<&str> = url_cleaned.split('?').collect();
    let host_port = parts[0];

    let mut streamid = "";
    if parts.len() > 1 {
        for param in parts[1].split('&') {
            let key_val: Vec<&str> = param.split('=').collect();
            if key_val.len() == 2 && key_val[0] == "streamid" {
                streamid = key_val[1];
            }
        }
    }

    let addr = match host_port.parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => {
            // Try resolving host
            if let Ok(mut addrs) = tokio::net::lookup_host(host_port).await {
                if let Some(a) = addrs.next() {
                    a
                } else {
                    eprintln!("[srt-egress] Failed to parse target URL: {}", target_url);
                    return;
                }
            } else {
                eprintln!(
                    "[srt-egress] Failed to parse/resolve target URL: {}",
                    target_url
                );
                return;
            }
        }
    };

    let client_sock = unsafe { srt_create_socket() };
    if client_sock < 0 {
        eprintln!("[srt-egress] Failed to create socket");
        return;
    }

    // Set streamid if present
    if !streamid.is_empty() {
        let streamid_c = match std::ffi::CString::new(streamid) {
            Ok(c) => c,
            Err(_) => {
                eprintln!("[srt-egress] Invalid stream ID (contains null byte)");
                unsafe {
                    srt_close(client_sock);
                }
                return;
            }
        };
        unsafe {
            srt_setsockopt(
                client_sock,
                0,
                SRTO_STREAMID,
                streamid_c.as_ptr() as *const c_void,
                streamid.len() as c_int,
            );
        }
    }

    let sin = to_sockaddr_in(addr);
    let conn_res = unsafe {
        srt_connect(
            client_sock,
            &sin,
            std::mem::size_of::<sockaddr_in>() as c_int,
        )
    };
    if conn_res < 0 {
        eprintln!("[srt-egress] Connection failed to {}", target_url);
        unsafe {
            srt_close(client_sock);
        }
        return;
    }

    println!("[srt-egress] Connected to {}", target_url);

    let mut reader = Reader::new(ring_buffer);
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                while let Ok(Some(packet)) = reader.pull() {
                    // Send MPEG-TS wrapping of elementary packet
                    // To do this simply, we write the payload of the packet over SRT.
                    // Egress expects the ring buffer to contain format-ready payloads for target protocol,
                    // or we transcode/remux them beforehand.
                    let payload = &packet.payload;
                    let send_res = unsafe { srt_send(client_sock, payload.as_ptr(), payload.len() as c_int) };
                    if send_res < 0 {
                        break;
                    }

                    // Update stats
                    engine.update_egress_bytes(&output_id, payload.len() as u64).await;
                }
            }
        }
    }

    unsafe {
        srt_close(client_sock);
    }
}
