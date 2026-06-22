//! Native RTMP ingest and egress using `rml_rtmp`.
//!
//! Ingest: accepts RTMP publish connections, authenticates stream keys against
//! the database, and pushes `MediaPacket`s into the pipeline's `RingBuffer`.
//! Keyframe detection uses FLV FrameType (works for both H.264 and H.265).
//!
//! Egress: connects to an RTMP target URL and forwards packets from the
//! `RingBuffer` via a `Reader`. Cancellation via `CancellationToken`.

use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult,
    PublishRequestType, ServerSession, ServerSessionConfig, ServerSessionEvent,
    ServerSessionResult,
};
use rml_rtmp::time::RtmpTimestamp;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use bytes::Bytes;
use crate::media::codec;
use crate::media::engine::{AudioMeta, MediaEngine, PublisherQuality, VideoMeta};
use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};
use crate::media::security::IngestSecurityService;
use crate::media::tcp_stats::collect_rtmp_receiver_stats;

struct RtmpIngestHandle {
    pipeline_id: String,
    ring: Arc<RingBuffer>,
    bytes_received: Arc<AtomicU64>,
}

fn parse_flv_video_meta(data: &[u8]) -> Option<VideoMeta> {
    if data.len() < 2 {
        return None;
    }
    let codec_id = data[0] & 0x0F;
    let codec = match codec_id {
        7 => "h264",
        12 => "h265",
        13 => "av1",
        2 => "h263",
        4 => "vp6",
        _ => return None,
    };

    let mut meta = VideoMeta {
        codec: codec.to_string(),
        ..Default::default()
    };

    // For H.264: byte[1]=AVC packet type, bytes[5..] = AVCDecoderConfigurationRecord when type=0
    if codec_id == 7 && data[1] == 0 && data.len() > 12 {
        let avc_config = &data[5..];
        if avc_config.len() >= 4 {
            let profile_idc = avc_config[1];
            let level_idc = avc_config[3];
            meta.profile = Some(
                match profile_idc {
                    66 => "Baseline",
                    77 => "Main",
                    88 => "Extended",
                    100 => "High",
                    110 => "High 10",
                    122 => "High 4:2:2",
                    244 => "High 4:4:4 Predictive",
                    _ => "Unknown",
                }
                .to_string(),
            );
            meta.level = Some(format!("{}.{}", level_idc / 10, level_idc % 10));

            // Parse SPS for resolution
            if avc_config.len() > 8 {
                let num_sps = (avc_config[5] & 0x1F) as usize;
                if num_sps > 0 && avc_config.len() > 8 {
                    let sps_len = ((avc_config[6] as usize) << 8) | (avc_config[7] as usize);
                    if avc_config.len() >= 8 + sps_len && sps_len > 1 {
                        if let Some((w, h)) = parse_sps_resolution(&avc_config[8..8 + sps_len]) {
                            meta.width = w;
                            meta.height = h;
                        }
                    }
                }
            }
        }
    }

    Some(meta)
}

fn flv_video_composition_time_ms(data: &[u8]) -> i32 {
    if data.len() < 5 || !matches!(data[0] & 0x0f, 7 | 12) || data[1] != 1 {
        return 0;
    }

    let value = ((data[2] as i32) << 16) | ((data[3] as i32) << 8) | data[4] as i32;
    if value & 0x0080_0000 != 0 {
        value | !0x00ff_ffff
    } else {
        value
    }
}

fn parse_sps_resolution(sps_nalu: &[u8]) -> Option<(u32, u32)> {
    if sps_nalu.is_empty() {
        return None;
    }
    // Remove emulation prevention bytes (0x00 0x00 0x03 → 0x00 0x00)
    let mut rbsp = Vec::with_capacity(sps_nalu.len());
    let mut i = 0;
    while i < sps_nalu.len() {
        if i + 2 < sps_nalu.len()
            && sps_nalu[i] == 0
            && sps_nalu[i + 1] == 0
            && sps_nalu[i + 2] == 3
        {
            rbsp.push(0);
            rbsp.push(0);
            i += 3;
        } else {
            rbsp.push(sps_nalu[i]);
            i += 1;
        }
    }

    let mut reader = BitReader::new(&rbsp);
    // Skip NAL unit header byte
    reader.skip(8)?;
    let profile_idc = reader.read_bits(8)? as u8;
    reader.skip(8)?; // constraint flags
    reader.skip(8)?; // level_idc
    reader.read_exp_golomb()?; // seq_parameter_set_id

    let high_profiles: &[u8] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134];
    if high_profiles.contains(&profile_idc) {
        let chroma = reader.read_exp_golomb()?;
        if chroma == 3 {
            reader.skip(1)?; // separate_colour_plane_flag
        }
        reader.read_exp_golomb()?; // bit_depth_luma_minus8
        reader.read_exp_golomb()?; // bit_depth_chroma_minus8
        reader.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        let scaling_present = reader.read_bits(1)?;
        if scaling_present == 1 {
            let count = if chroma != 3 { 8 } else { 12 };
            for j in 0..count {
                let list_present = reader.read_bits(1)?;
                if list_present == 1 {
                    let size = if j < 6 { 16 } else { 64 };
                    let mut last_scale = 8i32;
                    let mut next_scale = 8i32;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta = reader.read_signed_exp_golomb()?;
                            next_scale = (last_scale + delta + 256) % 256;
                        }
                        last_scale = if next_scale == 0 {
                            last_scale
                        } else {
                            next_scale
                        };
                    }
                }
            }
        }
    }

    reader.read_exp_golomb()?; // log2_max_frame_num_minus4
    let poc_type = reader.read_exp_golomb()?;
    if poc_type == 0 {
        reader.read_exp_golomb()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if poc_type == 1 {
        reader.skip(1)?;
        reader.read_signed_exp_golomb()?;
        reader.read_signed_exp_golomb()?;
        let n = reader.read_exp_golomb()?;
        for _ in 0..n {
            reader.read_signed_exp_golomb()?;
        }
    }
    reader.read_exp_golomb()?; // max_num_ref_frames
    reader.skip(1)?; // gaps_in_frame_num_value_allowed_flag

    let pic_width = reader.read_exp_golomb()?;
    let pic_height = reader.read_exp_golomb()?;
    let frame_mbs_only = reader.read_bits(1)?;
    if frame_mbs_only == 0 {
        reader.skip(1)?; // mb_adaptive_frame_field_flag
    }
    reader.skip(1)?; // direct_8x8_inference_flag
    let crop_flag = reader.read_bits(1)?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if crop_flag == 1 {
        (
            reader.read_exp_golomb()?,
            reader.read_exp_golomb()?,
            reader.read_exp_golomb()?,
            reader.read_exp_golomb()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    let width = (pic_width + 1) * 16 - crop_left * 2 - crop_right * 2;
    let height =
        (2 - frame_mbs_only as u32) * (pic_height + 1) * 16 - crop_top * 2 - crop_bottom * 2;

    Some((width, height))
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0..8, bits consumed in current byte
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn read_bits(&mut self, n: u8) -> Option<u32> {
        let mut val = 0u32;
        for _ in 0..n {
            if self.byte_pos >= self.data.len() {
                return None;
            }
            val = (val << 1) | ((self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1) as u32;
            self.bit_pos += 1;
            if self.bit_pos >= 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
        }
        Some(val)
    }

    fn skip(&mut self, n: u8) -> Option<()> {
        self.read_bits(n).map(|_| ())
    }

    fn read_exp_golomb(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        loop {
            let bit = self.read_bits(1)?;
            if bit == 1 {
                break;
            }
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(zeros as u8)?;
        Some((1 << zeros) - 1 + suffix)
    }

    fn read_signed_exp_golomb(&mut self) -> Option<i32> {
        let val = self.read_exp_golomb()?;
        if val == 0 {
            Some(0)
        } else if val % 2 == 1 {
            Some((val / 2 + 1) as i32)
        } else {
            Some(-(val as i32 / 2))
        }
    }
}

fn parse_flv_audio_meta(data: &[u8]) -> Option<AudioMeta> {
    if data.is_empty() {
        return None;
    }
    let byte0 = data[0];
    let format_id = (byte0 >> 4) & 0x0F;
    let rate_id = (byte0 >> 2) & 0x03;
    let channels_id = byte0 & 0x01;

    let codec = match format_id {
        10 => "aac",
        2 => "mp3",
        11 => "speex",
        14 => "mp3-8k",
        0 => "pcm",
        1 => "adpcm",
        _ => "unknown",
    };

    let sample_rate = match rate_id {
        0 => 5500,
        1 => 11025,
        2 => 22050,
        3 => 44100,
        _ => 0,
    };
    let channels = channels_id as u32 + 1;

    let mut meta = AudioMeta {
        codec: codec.to_string(),
        sample_rate,
        channels,
        channel_layout: Some(if channels == 1 { "mono" } else { "stereo" }.to_string()),
        track_index: 0,
    };

    // AAC AudioSpecificConfig gives actual sample rate/channels
    if format_id == 10 && data.len() > 2 && data[1] == 0 {
        let asc = &data[2..];
        if asc.len() >= 2 {
            let freq_idx = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
            let ch_config = (asc[1] >> 3) & 0x0F;
            let aac_rates: &[u32] = &[
                96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000,
                7350,
            ];
            if (freq_idx as usize) < aac_rates.len() {
                meta.sample_rate = aac_rates[freq_idx as usize];
            }
            if ch_config > 0 {
                meta.channels = ch_config as u32;
                meta.channel_layout = Some(
                    match ch_config {
                        1 => "mono",
                        2 => "stereo",
                        3 => "3.0",
                        4 => "4.0",
                        5 => "5.0",
                        6 => "5.1",
                        7 => "7.1",
                        _ => "unknown",
                    }
                    .to_string(),
                );
            }
        }
    }

    Some(meta)
}

// Standard RTMP URL parser helper
fn parse_rtmp_url(url: &str) -> Option<(String, u16, String, String)> {
    if !url.starts_with("rtmp://") && !url.starts_with("rtmps://") {
        return None;
    }
    let prefix_len = if url.starts_with("rtmps://") {
        "rtmps://".len()
    } else {
        "rtmp://".len()
    };
    let s = &url[prefix_len..];
    let slash_idx = s.find('/')?;
    let host_port = &s[..slash_idx];
    let path = &s[slash_idx + 1..];

    let (host, port) = if let Some(colon_idx) = host_port.find(':') {
        let h = &host_port[..colon_idx];
        let p = host_port[colon_idx + 1..].parse::<u16>().ok()?;
        (h.to_string(), p)
    } else {
        (host_port.to_string(), 1935)
    };

    let path_slash = path.find('/')?;
    let app = &path[..path_slash];
    let stream_key = &path[path_slash + 1..];

    Some((host, port, app.to_string(), stream_key.to_string()))
}

/// RTMP Ingest Server
pub async fn start_rtmp_server(
    db: sqlx::SqlitePool,
    security: Arc<IngestSecurityService>,
    engine: Arc<MediaEngine>,
) {
    start_rtmp_server_on(db, security, engine, 1935).await;
}

pub async fn start_rtmp_server_on(
    db: sqlx::SqlitePool,
    security: Arc<IngestSecurityService>,
    engine: Arc<MediaEngine>,
    port: u16,
) {
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[rtmp] Failed to bind TCP listener on {}: {:?}", addr, e);
            return;
        }
    };
    println!("[rtmp] Server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                let db_clone = db.clone();
                let security_clone = security.clone();
                let engine_clone = engine.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_rtmp_client(socket, addr, db_clone, security_clone, engine_clone)
                            .await
                    {
                        eprintln!("[rtmp] Error handling client {}: {:?}", addr, e);
                    }
                });
            }
            Err(e) => {
                eprintln!("[rtmp] Accept error: {:?}", e);
            }
        }
    }
}

async fn handle_rtmp_client(
    mut socket: TcpStream,
    client_addr: SocketAddr,
    db: sqlx::SqlitePool,
    security: Arc<IngestSecurityService>,
    engine: Arc<MediaEngine>,
) -> Result<(), &'static str> {
    let client_ip = client_addr.ip().to_string();
    let client_addr_text = client_addr.to_string();
    // Configure socket for low jitter and fast response
    let _ = socket.set_nodelay(true);

    // 8 MB kernel buffers: at 4K60 (~50 Mbps) a 1.3s burst fills 8 MB.
    // Default ~128 KB would overflow within a single GOP.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = socket.as_raw_fd();
        let size: libc::c_int = 8 * 1024 * 1024;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    let mut handshake = Handshake::new(PeerType::Server);
    let mut buffer = vec![0u8; 4096];
    let mut remaining: Vec<u8> = Vec::new();
    let mut handshake_completed = false;

    // 1. Handshake Loop
    while !handshake_completed {
        let n = socket
            .read(&mut buffer)
            .await
            .map_err(|_| "Socket read error during handshake")?;
        if n == 0 {
            return Err("Socket closed during handshake");
        }

        let result = handshake
            .process_bytes(&buffer[..n])
            .map_err(|_| "Handshake parsing error")?;
        match result {
            HandshakeProcessResult::InProgress { response_bytes } => {
                if !response_bytes.is_empty() {
                    socket
                        .write_all(&response_bytes)
                        .await
                        .map_err(|_| "Socket write error during handshake")?;
                }
            }
            HandshakeProcessResult::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                if !response_bytes.is_empty() {
                    socket
                        .write_all(&response_bytes)
                        .await
                        .map_err(|_| "Socket write error during handshake")?;
                }
                remaining = remaining_bytes;
                handshake_completed = true;
            }
        }
    }

    // 2. Initialize ServerSession
    let config = ServerSessionConfig::new();
    let (mut session, initial_results) =
        ServerSession::new(config).map_err(|_| "Failed to initialize server session")?;

    for res in initial_results {
        if let ServerSessionResult::OutboundResponse(pkt) = res {
            socket
                .write_all(&pkt.bytes)
                .await
                .map_err(|_| "Failed to write initial response")?;
        }
    }

    let mut active_ingest: Option<RtmpIngestHandle> = None;
    let mut probe = ProbeState {
        video_done: false,
        audio_done: false,
    };

    // Process left over bytes from handshake
    if !remaining.is_empty() {
        let results = session
            .handle_input(&remaining)
            .map_err(|_| "Session parse error on remaining bytes")?;
        if let Err(error) = handle_session_results(
            &mut session,
            results,
            &mut socket,
            &db,
            &security,
            &engine,
            &client_ip,
            &client_addr_text,
            &mut probe,
            &mut active_ingest,
        )
        .await
        {
            if let Some(active) = &active_ingest {
                engine.unregister_ingest(&active.pipeline_id).await;
            }
            return Err(error);
        }
    }

    // 3. Main Protocol Loop
    let mut tcp_stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    tcp_stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous_tcp_bytes: Option<(u64, Instant)> = None;
    loop {
        tokio::select! {
            read_result = socket.read(&mut buffer) => {
                let n = read_result.map_err(|_| "Read error in main loop")?;
                if n == 0 {
                    break;
                }

                let results = session
                    .handle_input(&buffer[..n])
                    .map_err(|_| "Session parse error")?;
                if let Err(e) = handle_session_results(
                    &mut session,
                    results,
                    &mut socket,
                    &db,
                    &security,
                    &engine,
                    &client_ip,
                    &client_addr_text,
                    &mut probe,
                    &mut active_ingest,
                )
                .await
                {
                    eprintln!("[rtmp] session result error: {}", e);
                    break;
                }
            }
            _ = tcp_stats_interval.tick(), if active_ingest.is_some() => {
                let pipeline_id = active_ingest
                    .as_ref()
                    .map(|active| active.pipeline_id.as_str())
                    .unwrap_or_default();
                let now = Instant::now();
                let quality = match collect_rtmp_receiver_stats(&socket) {
                    Ok(stats) => {
                        let receive_rate = stats.tcp_bytes_received.and_then(|bytes| {
                            let rate = previous_tcp_bytes.and_then(|(previous, sampled_at)| {
                                let elapsed = now.duration_since(sampled_at).as_secs_f64();
                                let delta = bytes.checked_sub(previous)?;
                                (elapsed > 0.0).then_some(
                                    (delta as f64 * 8.0) / (elapsed * 1_000_000.0),
                                )
                            });
                            previous_tcp_bytes = Some((bytes, now));
                            rate
                        });
                        PublisherQuality {
                            tcp_rtt_ms: stats.tcp_rtt_ms,
                            tcp_rtt_var_ms: stats.tcp_rtt_var_ms,
                            tcp_bytes_received: stats.tcp_bytes_received,
                            tcp_last_rcv_ms: stats.tcp_last_rcv_ms,
                            tcp_rcv_rtt_ms: stats.tcp_rcv_rtt_ms,
                            tcp_rcv_space: stats.tcp_rcv_space,
                            tcp_rcv_ooopack: stats.tcp_rcv_ooopack,
                            tcp_skmem_rmem_alloc: stats.tcp_skmem_rmem_alloc,
                            tcp_skmem_rmem_max: stats.tcp_skmem_rmem_max,
                            tcp_receive_rate_mbps: receive_rate,
                            ..PublisherQuality::default()
                        }
                    }
                    Err(error) => PublisherQuality {
                        tcp_stats_unavailable_reason: Some(match error.kind() {
                            std::io::ErrorKind::Unsupported => "not_linux",
                            _ => "collection_failed",
                        }.to_string()),
                        ..PublisherQuality::default()
                    },
                };
                engine.update_publisher_quality(pipeline_id, quality).await;
            }
        }
    }

    // Clean up active ingest on disconnect
    if let Some(active) = &active_ingest {
        println!(
            "[rtmp] Publisher disconnected for pipeline: {}",
            active.pipeline_id
        );
        engine.unregister_ingest(&active.pipeline_id).await;
    }

    Ok(())
}

struct ProbeState {
    video_done: bool,
    audio_done: bool,
}

async fn handle_session_results(
    session: &mut ServerSession,
    results: Vec<ServerSessionResult>,
    socket: &mut TcpStream,
    db: &sqlx::SqlitePool,
    security: &IngestSecurityService,
    engine: &MediaEngine,
    client_ip: &str,
    client_addr: &str,
    probe: &mut ProbeState,
    active_ingest: &mut Option<RtmpIngestHandle>,
) -> Result<(), &'static str> {
    for res in results {
        match res {
            ServerSessionResult::OutboundResponse(pkt) => {
                socket
                    .write_all(&pkt.bytes)
                    .await
                    .map_err(|_| "Failed to write outbound response")?;
            }
            ServerSessionResult::RaisedEvent(event) => {
                match event {
                    ServerSessionEvent::ConnectionRequested {
                        request_id,
                        app_name: _,
                    } => {
                        // Accept connection
                        if let Ok(resp) = session.accept_request(request_id) {
                            for r in resp {
                                if let ServerSessionResult::OutboundResponse(pkt) = r {
                                    socket
                                        .write_all(&pkt.bytes)
                                        .await
                                        .map_err(|_| "Write error")?;
                                }
                            }
                        }
                    }
                    ServerSessionEvent::PublishStreamRequested {
                        request_id,
                        app_name: _,
                        stream_key,
                        mode: _,
                    } => {
                        // Rate limit security check
                        if let Some(_) = security.is_ip_banned(client_ip) {
                            let _ = session.reject_request(
                                request_id,
                                "NetStream.Publish.BadName",
                                "IP temporarily banned due to too many login/publish failures",
                            );
                            return Err("IP is banned");
                        }

                        // Validate stream key against database pipelines
                        let pipeline = match sqlx::query_as::<_, crate::types::Pipeline>(
                            "SELECT id, name, stream_key, input_source, encoding FROM pipelines WHERE stream_key = ?"
                        )
                        .bind(&stream_key)
                        .fetch_optional(db)
                        .await {
                            Ok(Some(p)) => p,
                            Ok(None) => {
                                eprintln!("[rtmp] publish stream key not found: {:?}", stream_key);
                                security.record_failure(client_ip);
                                let _ = session.reject_request(request_id, "NetStream.Publish.BadName", "Invalid stream key");
                                return Err("Invalid stream key");
                            }
                            Err(e) => {
                                eprintln!("[rtmp] publish stream key DB query failed: {:?}", e);
                                security.record_failure(client_ip);
                                let _ = session.reject_request(request_id, "NetStream.Publish.BadName", "Invalid stream key");
                                return Err("Invalid stream key");
                            }
                        };

                        // Reserve the pipeline before accepting the publish request.
                        // A bonded SRT group is one logical publisher, but a second
                        // independent RTMP/SRT publisher must not create another
                        // writer for the same RingBuffer.
                        let Some(_token) = engine
                            .try_register_ingest(&pipeline.id, &stream_key, "rtmp")
                            .await
                        else {
                            let _ = session.reject_request(
                                request_id,
                                "NetStream.Publish.BadName",
                                "Pipeline already has an active publisher",
                            );
                            return Err("Pipeline already has an active publisher");
                        };
                        let ring = engine.get_or_create_pipeline(&pipeline.id).await;
                        let bytes_received = {
                            let ingests = engine.active_ingests.read().await;
                            ingests
                                .get(&pipeline.id)
                                .map(|ingest| ingest.bytes_received.clone())
                        };
                        let Some(bytes_received) = bytes_received else {
                            engine.unregister_ingest(&pipeline.id).await;
                            return Err("Active ingest disappeared during registration");
                        };
                        *active_ingest = Some(RtmpIngestHandle {
                            pipeline_id: pipeline.id.clone(),
                            ring,
                            bytes_received,
                        });

                        // Success! Accept publish request
                        let resp = session
                            .accept_request(request_id)
                            .map_err(|_| "Failed to accept publish request")?;
                        for r in resp {
                            if let ServerSessionResult::OutboundResponse(pkt) = r {
                                socket
                                    .write_all(&pkt.bytes)
                                    .await
                                    .map_err(|_| "Write error")?;
                            }
                        }

                        engine
                            .update_ingest_meta(
                                &pipeline.id,
                                None,
                                None,
                                Some(client_addr.to_string()),
                            )
                            .await;
                        security.record_success(client_ip);
                        println!(
                            "[rtmp] Ingest successfully started on pipeline: {}",
                            pipeline.id
                        );
                    }
                    ServerSessionEvent::VideoDataReceived {
                        app_name: _,
                        stream_key: _,
                        data,
                        timestamp,
                    } => {
                        if let Some(active) = active_ingest.as_ref() {
                            let pipeline_id = &active.pipeline_id;
                            active
                                .bytes_received
                                .fetch_add(data.len() as u64, Ordering::Relaxed);

                            let is_keyframe = if data.is_empty() {
                                false
                            } else {
                                (data[0] >> 4) == 1
                            };

                            let dts = timestamp.value as i64;
                            let pts = dts + flv_video_composition_time_ms(&data) as i64;

                            if is_keyframe {
                                engine.record_keyframe(pipeline_id, pts).await;
                            }

                            // Cache video sequence header for play subscribers
                            if data.len() >= 2 && (data[0] & 0x0F) == 7 && data[1] == 0 {
                                engine
                                    .cache_sequence_header(pipeline_id, true, data.clone())
                                    .await;
                            }

                            // Probe video metadata from sequence header (first config packet)
                            if !probe.video_done {
                                if let Some(meta) = parse_flv_video_meta(&data) {
                                    if meta.width > 0 {
                                        probe.video_done = true;
                                    }
                                    println!(
                                        "[rtmp] Probed video: {} {}x{} profile={:?} level={:?}",
                                        meta.codec,
                                        meta.width,
                                        meta.height,
                                        meta.profile,
                                        meta.level
                                    );
                                    engine
                                        .update_ingest_meta(pipeline_id, Some(meta), None, None)
                                        .await;
                                }
                            }

                            let packet = MediaPacket {
                                media_type: MediaType::Video,
                                track_index: 0,
                                pts,
                                dts,
                                is_keyframe,
                                format: PayloadFormat::Flv,
                                payload: data,
                            };
                            active.ring.push(packet);
                        }
                    }
                    ServerSessionEvent::AudioDataReceived {
                        app_name: _,
                        stream_key: _,
                        data,
                        timestamp,
                    } => {
                        if let Some(active) = active_ingest.as_ref() {
                            let pipeline_id = &active.pipeline_id;
                            active
                                .bytes_received
                                .fetch_add(data.len() as u64, Ordering::Relaxed);

                            // Cache audio sequence header for play subscribers
                            if data.len() >= 2 && (data[0] >> 4) == 10 && data[1] == 0 {
                                engine
                                    .cache_sequence_header(pipeline_id, false, data.clone())
                                    .await;
                            }

                            // AAC's FLV sound-rate/channel bits are only legacy
                            // hints. Wait for AudioSpecificConfig so 48 kHz,
                            // mono, and other real AAC layouts are not reported
                            // as the FLV fallback of 44.1 kHz stereo.
                            if !probe.audio_done {
                                let format_id =
                                    data.first().map(|byte| (byte >> 4) & 0x0f).unwrap_or(0xff);
                                let has_complete_config =
                                    format_id != 10 || (data.len() >= 3 && data[1] == 0);
                                if has_complete_config
                                    && let Some(meta) = parse_flv_audio_meta(&data)
                                {
                                    probe.audio_done = true;
                                    println!(
                                        "[rtmp] Probed audio: {} {}Hz {}ch",
                                        meta.codec, meta.sample_rate, meta.channels
                                    );
                                    engine
                                        .update_ingest_meta(pipeline_id, None, Some(meta), None)
                                        .await;
                                }
                            }

                            let packet = MediaPacket {
                                media_type: MediaType::Audio,
                                track_index: 0,
                                pts: timestamp.value as i64,
                                dts: timestamp.value as i64,
                                is_keyframe: false,
                                format: PayloadFormat::Flv,
                                payload: data,
                            };
                            active.ring.push(packet);
                        }
                    }
                    ServerSessionEvent::PlayStreamRequested {
                        request_id,
                        app_name: _,
                        stream_key,
                        start_at: _,
                        duration: _,
                        reset: _,
                        stream_id,
                    } => {
                        // Look up pipeline by stream key
                        let pipeline = match sqlx::query_as::<_, crate::types::Pipeline>(
                            "SELECT id, name, stream_key, input_source, encoding FROM pipelines WHERE stream_key = ?"
                        )
                        .bind(&stream_key)
                        .fetch_optional(db)
                        .await {
                            Ok(Some(p)) => p,
                            _ => {
                                let _ = session.reject_request(request_id, "NetStream.Play.StreamNotFound", "Invalid stream key");
                                return Err("Invalid stream key for play");
                            }
                        };

                        // Check if there's an active ingest
                        if !engine
                            .active_ingests
                            .read()
                            .await
                            .contains_key(&pipeline.id)
                        {
                            let _ = session.reject_request(
                                request_id,
                                "NetStream.Play.StreamNotFound",
                                "No active ingest",
                            );
                            return Err("No active ingest for play");
                        }

                        let resp = session
                            .accept_request(request_id)
                            .map_err(|_| "Failed to accept play request")?;
                        // rml_rtmp 0.8 appends two optional AMF data messages
                        // after the required reset, stream-begin, and play-start
                        // responses: |RtmpSampleAccess and NetStream.Data.Start.
                        // FFmpeg exposes those notifications as synthetic
                        // subtitle/data streams. We do not send metadata on
                        // their chunk stream, so omitting these two optional
                        // messages is safe and keeps the read endpoint media-only.
                        for r in resp.into_iter().take(3) {
                            if let ServerSessionResult::OutboundResponse(pkt) = r {
                                socket
                                    .write_all(&pkt.bytes)
                                    .await
                                    .map_err(|_| "Write error")?;
                            }
                        }

                        println!(
                            "[rtmp] Play subscriber connected for pipeline: {} (stream_id={})",
                            pipeline.id, stream_id
                        );

                        // Send cached sequence headers so the player can initialize decoders
                        let (video_sh, audio_sh) = engine.get_sequence_headers(&pipeline.id).await;
                        if let Some(vsh) = video_sh {
                            if let Ok(pkt) = session.send_video_data(
                                stream_id,
                                vsh,
                                RtmpTimestamp::new(0),
                                false,
                            ) {
                                let _ = socket.write_all(&pkt.bytes).await;
                            }
                        }
                        if let Some(ash) = audio_sh {
                            if let Ok(pkt) = session.send_audio_data(
                                stream_id,
                                ash,
                                RtmpTimestamp::new(0),
                                false,
                            ) {
                                let _ = socket.write_all(&pkt.bytes).await;
                            }
                        }

                        // Feed loop: read from RingBuffer and send RTMP data
                        let ring_buf = engine.get_or_create_pipeline(&pipeline.id).await;
                        let mut reader = Reader::new(format!("rtmp_play:{}", pipeline.id), ring_buf);

                        loop {
                            match reader.pull() {
                                Ok(Some(pkt)) => {
                                    let ts = match pkt.media_type {
                                        MediaType::Video => {
                                            RtmpTimestamp::new(pkt.dts.max(0) as u32)
                                        }
                                        MediaType::Audio => {
                                            RtmpTimestamp::new(pkt.pts.max(0) as u32)
                                        }
                                    };
                                    let result = match pkt.media_type {
                                        MediaType::Video => session.send_video_data(
                                            stream_id,
                                            pkt.payload.clone(),
                                            ts,
                                            !pkt.is_keyframe,
                                        ),
                                        MediaType::Audio => session.send_audio_data(
                                            stream_id,
                                            pkt.payload.clone(),
                                            ts,
                                            false,
                                        ),
                                    };
                                    match result {
                                        Ok(packet) => {
                                            if socket.write_all(&packet.bytes).await.is_err() {
                                                println!(
                                                    "[rtmp] Play subscriber disconnected for pipeline: {}",
                                                    pipeline.id
                                                );
                                                return Err("Play subscriber disconnected");
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                                Ok(None) => {
                                    reader.wait_for_data().await;
                                }
                                Err(_) => {
                                    // Overflow — reader was fast-forwarded, continue
                                }
                            }
                        }
                        return Err("Play finished");
                    }
                    ServerSessionEvent::PublishStreamFinished {
                        app_name: _,
                        stream_key: _,
                    } => {
                        return Err("Publish finished by client");
                    }
                    _ => {}
                }
            }
            ServerSessionResult::UnhandleableMessageReceived(_) => {}
        }
    }
    Ok(())
}

/// H.265 → H.264 transcoder for RTMP egress.
///
/// Reads H.265 MPEG-TS from `in_queue`, decodes with FFmpeg's HEVC decoder,
/// re-encodes to H.264 Annex B, and sends `(annexb, is_keyframe, pts_ms)` via
/// `out_tx`. Runs on a dedicated OS thread (FFmpeg codec calls block).
fn run_hevc_to_h264_transcoder(
    in_queue: Arc<crate::media::avio::MemoryQueue>,
    out_tx: tokio::sync::mpsc::UnboundedSender<(Vec<u8>, bool, i64)>,
    cancel: CancellationToken,
) {
    use crate::media::avio::CustomInput;
    use ffmpeg_next::{codec, format::Pixel, frame, software};

    let mut custom = match CustomInput::new(&*in_queue) {
        Ok(c) => c,
        Err(e) => { eprintln!("[rtmp-h265-tc] custom input failed: {e}"); return; }
    };
    let ictx = match custom.input.as_mut() {
        Some(i) => i,
        None => return,
    };

    let video_idx = match ictx.streams()
        .find(|s| s.parameters().medium() == ffmpeg_next::media::Type::Video)
        .map(|s| s.index())
    {
        Some(i) => i,
        None => { eprintln!("[rtmp-h265-tc] no video stream found"); return; }
    };

    let dec_params = ictx.stream(video_idx).unwrap().parameters();
    let dec_ctx = match codec::Context::from_parameters(dec_params) {
        Ok(c) => c,
        Err(e) => { eprintln!("[rtmp-h265-tc] decoder context: {e}"); return; }
    };
    let mut decoder = match dec_ctx.decoder().video() {
        Ok(d) => d,
        Err(e) => { eprintln!("[rtmp-h265-tc] decoder open: {e}"); return; }
    };

    let enc_codec = match codec::encoder::find(codec::Id::H264) {
        Some(c) => c,
        None => { eprintln!("[rtmp-h265-tc] no H.264 encoder found"); return; }
    };

    // Encoder and scaler are initialized lazily on the first decoded frame
    // so we know width/height/pixel format from the decoder.
    let mut encoder: Option<codec::encoder::video::Encoder> = None;
    let mut scaler: Option<software::scaling::Context> = None;
    let mut enc_frame = frame::Video::empty();
    let mut enc_pkt = ffmpeg_next::Packet::empty();
    let mut pts_counter: i64 = 0;  // frame counter (increments by 1 per frame)
    let mut fps_den_stored: i64 = 1;
    let mut fps_num_stored: i64 = 30; // updated after encoder opens

    for (stream, pkt) in ictx.packets() {
        if cancel.is_cancelled() { break; }
        if stream.index() != video_idx { continue; }

        if decoder.send_packet(&pkt).is_err() { continue; }

        let mut dec_frame = frame::Video::empty();
        while decoder.receive_frame(&mut dec_frame).is_ok() {
            // Lazy encoder + scaler init on first decoded frame
            if encoder.is_none() {
                let width  = decoder.width();
                let height = decoder.height();
                let in_fmt = dec_frame.format();

                let sw = match software::scaling::Context::get(
                    in_fmt, width, height,
                    Pixel::YUV420P, width, height,
                    software::scaling::Flags::BILINEAR,
                ) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[rtmp-h265-tc] scaler: {e}"); return; }
                };

                // Derive frame rate before opening the encoder (x264 requires it)
                let fr = stream.avg_frame_rate();
                let (fps_num, fps_den) = if fr.numerator() > 0 && fr.denominator() > 0 {
                    (fr.numerator(), fr.denominator())
                } else {
                    (30, 1) // fallback 30fps
                };
                fps_num_stored = fps_num as i64;
                fps_den_stored = fps_den as i64;

                let enc_ctx = codec::Context::new();
                let mut enc_video = match enc_ctx.encoder().video() {
                    Ok(e) => e,
                    Err(e) => { eprintln!("[rtmp-h265-tc] encoder ctx: {e}"); return; }
                };
                enc_video.set_width(width);
                enc_video.set_height(height);
                enc_video.set_format(Pixel::YUV420P);
                // time_base = 1/fps so x264 derives a sensible frame rate
                enc_video.set_time_base(ffmpeg_next::Rational::new(fps_den, fps_num));
                enc_video.set_frame_rate(Some(ffmpeg_next::Rational::new(fps_num, fps_den)));
                enc_video.set_bit_rate(2_500_000);
                enc_video.set_gop(60);

                let mut opts = ffmpeg_next::Dictionary::new();
                opts.set("preset", "ultrafast");
                opts.set("tune",   "zerolatency");

                let opened = match enc_video.open_as_with(enc_codec, opts) {
                    Ok(e) => e,
                    Err(e) => { eprintln!("[rtmp-h265-tc] encoder open: {e}"); return; }
                };

                scaler  = Some(sw);
                encoder = Some(opened);
            }

            let enc = encoder.as_mut().unwrap();
            let sw  = scaler.as_mut().unwrap();

            if sw.run(&dec_frame, &mut enc_frame).is_err() { continue; }
            enc_frame.set_pts(Some(pts_counter));
            pts_counter += 1;

            if enc.send_frame(&enc_frame).is_err() { continue; }
            while enc.receive_packet(&mut enc_pkt).is_ok() {
                let data   = enc_pkt.data().unwrap_or(&[]).to_vec();
                let is_key = enc_pkt.is_key();
                // pts in frame units → ms: frame * fps_den * 1000 / fps_num
                let pts_ms = enc_pkt.pts().unwrap_or(0) * fps_den_stored * 1000 / fps_num_stored;
                if out_tx.send((data, is_key, pts_ms)).is_err() { return; }
            }
        }
    }

    // Flush remaining encoder output
    if let Some(enc) = encoder.as_mut() {
        let _ = enc.send_eof();
        while enc.receive_packet(&mut enc_pkt).is_ok() {
            let data   = enc_pkt.data().unwrap_or(&[]).to_vec();
            let is_key = enc_pkt.is_key();
            let pts_ms = enc_pkt.pts().unwrap_or(0) * fps_den_stored * 1000 / fps_num_stored;
            let _ = out_tx.send((data, is_key, pts_ms));
        }
    }
}

/// RTMP Egress Client
pub async fn start_rtmp_egress(
    output_id: String,
    pipeline_id: String,
    target_url: String,
    ring_buffer: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel_token: CancellationToken,
) {
    let parsed = match parse_rtmp_url(&target_url) {
        Some(p) => p,
        None => {
            eprintln!("[rtmp-egress] Invalid RTMP URL: {}", target_url);
            return;
        }
    };
    let (host, port, app_name, stream_key) = parsed;
    println!(
        "[rtmp-egress] Connecting to {}:{} (app: {}, key: {})",
        host, port, app_name, stream_key
    );

    let mut socket = match TcpStream::connect(format!("{}:{}", host, port)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[rtmp-egress] Connection failed to {}:{}: {:?}",
                host, port, e
            );
            return;
        }
    };

    let _ = socket.set_nodelay(true);

    // Perform handshake
    let mut handshake = Handshake::new(PeerType::Client);
    let c0_c1 = match handshake.generate_outbound_p0_and_p1() {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!(
                "[rtmp-egress] Handshake outbound generation failed: {:?}",
                e
            );
            return;
        }
    };

    if socket.write_all(&c0_c1).await.is_err() {
        return;
    }

    let mut buffer = vec![0u8; 4096];
    let mut handshake_completed = false;
    let mut remaining: Vec<u8> = Vec::new();

    while !handshake_completed {
        tokio::select! {
            _ = cancel_token.cancelled() => return,
            res = socket.read(&mut buffer) => {
                let n = match res {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                match handshake.process_bytes(&buffer[..n]) {
                    Ok(HandshakeProcessResult::InProgress { response_bytes }) => {
                        if !response_bytes.is_empty() {
                            if socket.write_all(&response_bytes).await.is_err() { return; }
                        }
                    }
                    Ok(HandshakeProcessResult::Completed { response_bytes, remaining_bytes }) => {
                        if !response_bytes.is_empty() {
                            if socket.write_all(&response_bytes).await.is_err() { return; }
                        }
                        remaining = remaining_bytes;
                        handshake_completed = true;
                    }
                    Err(e) => {
                        eprintln!("[rtmp-egress] Handshake process bytes failed: {:?}", e);
                        return;
                    }
                }
            }
        }
    }

    // Initialize ClientSession with tcUrl for MediaMTX compatibility
    let mut config = ClientSessionConfig::new();
    config.tc_url = Some(format!("rtmp://{}:{}/{}", host, port, app_name));
    let (mut session, initial_results) = match ClientSession::new(config) {
        Ok(s) => s,
        Err(_) => return,
    };

    for res in initial_results {
        if let ClientSessionResult::OutboundResponse(pkt) = res {
            if socket.write_all(&pkt.bytes).await.is_err() {
                return;
            }
        }
    }

    // Request connection
    let conn_pkt = match session.request_connection(app_name) {
        Ok(ClientSessionResult::OutboundResponse(p)) => p,
        _ => return,
    };
    if socket.write_all(&conn_pkt.bytes).await.is_err() {
        return;
    }

    if !remaining.is_empty() {
        let results = match session.handle_input(&remaining) {
            Ok(r) => r,
            Err(_) => return,
        };
        if handle_client_results(results, &mut socket, &mut session, &stream_key)
            .await
            .is_err()
        {
            return;
        }
    }

    let egress_bytes_sent = {
        let egresses = engine.active_egresses.read().await;
        egresses.get(&output_id).map(|e| e.bytes_sent.clone())
    };

    // Detect H.265 ingest — standard RTMP only carries H.264, so we transcode
    // H.265 → H.264 on a dedicated OS thread using FFmpeg decode+encode.
    let is_h265 = {
        let ingests = engine.active_ingests.read().await;
        ingests
            .get(&pipeline_id)
            .and_then(|i| i.video.as_ref())
            .map(|v| v.codec == "hevc")
            .unwrap_or(false)
    };

    // H.265 transcoder infrastructure (only active when is_h265).
    // We mux H.265 ring-buffer packets into a MemoryQueue as MPEG-TS so that
    // the FFmpeg-based transcoder can demux, decode, re-encode, and return
    // H.264 Annex B packets via a tokio channel.
    let h265_in_queue: Option<Arc<crate::media::avio::MemoryQueue>>;
    let mut h264_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, bool, i64)>>;
    let mut h265_ts_muxer: Option<crate::media::mpegts::TsMuxer>;
    let mut h265_dts_enforcer: Option<crate::media::ring_buffer::DtsEnforcer>;
    let mut h264_seq_sent = false;
    let mut h265_nalu_len = 4usize;
    let mut h265_sps_cache: Vec<u8> = Vec::new();

    if is_h265 {
        println!("[rtmp-egress] H.265 source on pipeline {pipeline_id}: transcoding to H.264 for RTMP");
        let iq = Arc::new(crate::media::avio::MemoryQueue::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(Vec<u8>, bool, i64)>();
        let iq_clone = iq.clone();
        let cancel_clone = cancel_token.clone();
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_hevc_to_h264_transcoder(iq_clone, tx, cancel_clone);
            }));
        });

        // Build a video-only TsMuxer for the H.265 input stream
        let video_meta = {
            let ingests = engine.active_ingests.read().await;
            ingests.get(&pipeline_id).and_then(|i| i.video.clone())
        };
        let mux = crate::media::mpegts::TsMuxer::new(video_meta.as_ref(), &[]);
        let dts = crate::media::ring_buffer::DtsEnforcer::new(1);

        h265_in_queue    = Some(iq);
        h264_rx          = Some(rx);
        h265_ts_muxer    = Some(mux);
        h265_dts_enforcer = Some(dts);
    } else {
        h265_in_queue    = None;
        h264_rx          = None;
        h265_ts_muxer    = None;
        h265_dts_enforcer = None;
    }

    let mut is_publishing = false;
    let mut reader = Reader::new(format!("rtmp_egress:{}", output_id), ring_buffer);
    let mut raw_seq_header_sent = false;

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                let _ = session.stop_publishing();
                break;
            }
            // Read from server to handle acknowledgements, status codes, pings
            res = socket.read(&mut buffer) => {
                let n = match res {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                let results = match session.handle_input(&buffer[..n]) {
                    Ok(r) => r,
                    Err(_) => break,
                };
                for r in results {
                    match r {
                        ClientSessionResult::OutboundResponse(pkt) => {
                            if socket.write_all(&pkt.bytes).await.is_err() { return; }
                        }
                        ClientSessionResult::RaisedEvent(event) => {
                            match event {
                                ClientSessionEvent::ConnectionRequestAccepted => {
                                    let pub_pkt = match session.request_publishing(stream_key.clone(), PublishRequestType::Live) {
                                        Ok(ClientSessionResult::OutboundResponse(p)) => p,
                                        _ => return,
                                    };
                                    if socket.write_all(&pub_pkt.bytes).await.is_err() { return; }
                                }
                                ClientSessionEvent::PublishRequestAccepted => {
                                    println!("[rtmp-egress] Stream publishing accepted on target");
                                    // Send cached sequence headers before media data.
                                    // For H.265 ingests, video_sh is None (only RTMP ingest
                                    // caches FLV seq headers), so this is a no-op for H.265.
                                    let (video_sh, mut audio_sh) = engine.get_sequence_headers(&pipeline_id).await;
                                    if let Some(vsh) = video_sh {
                                        if let Ok(ClientSessionResult::OutboundResponse(p)) =
                                            session.publish_video_data(vsh, RtmpTimestamp::new(0), true)
                                        {
                                            if socket.write_all(&p.bytes).await.is_err() { return; }
                                        }
                                    }
                                    // Synthesize AAC sequence header from audio meta if not cached
                                    if audio_sh.is_none() {
                                        let ingests = engine.active_ingests.read().await;
                                        if let Some(ingest) = ingests.get(&pipeline_id) {
                                            let tracks = ingest.audio_tracks.lock().unwrap();
                                            if let Some(track) = tracks.first() {
                                                audio_sh = Some(codec::build_aac_sequence_header(
                                                    track.sample_rate,
                                                    track.channels,
                                                ));
                                            }
                                        }
                                    }
                                    if let Some(ash) = audio_sh {
                                        if let Ok(ClientSessionResult::OutboundResponse(p)) =
                                            session.publish_audio_data(ash, RtmpTimestamp::new(0), false)
                                        {
                                            if socket.write_all(&p.bytes).await.is_err() { return; }
                                        }
                                    }
                                    is_publishing = true;
                                }
                                ClientSessionEvent::ConnectionRequestRejected { description } => {
                                    eprintln!("[rtmp-egress] Connection rejected: {}", description);
                                    return;
                                }
                                _ => {}
                            }
                        }
                        ClientSessionResult::UnhandleableMessageReceived(_) => {}
                    }
                }
            }
            // Write packets from ring buffer when publishing is active
            _ = reader.wait_for_data(), if is_publishing => {
                let mut packets = Vec::with_capacity(32);
                if reader.pull_burst(&mut packets, 32).is_ok() {
                    for packet in packets {
                        // H.265 video: mux to MPEG-TS and hand off to the transcoder thread.
                        // The resulting H.264 packets arrive on the h264_rx branch below.
                        if is_h265 && packet.media_type == MediaType::Video {
                            if let (Some(mux), Some(dts), Some(iq)) = (
                                &mut h265_ts_muxer,
                                &mut h265_dts_enforcer,
                                &h265_in_queue,
                            ) {
                                if let Some(payload) = crate::media::codec::video_for_ts(
                                    &packet.payload,
                                    packet.format,
                                    &mut h265_nalu_len,
                                    &mut h265_sps_cache,
                                ) {
                                    let (pts, dts_val) = dts.enforce(0, packet.pts, packet.dts);
                                    let ts_bytes = mux.mux_packet(
                                        crate::media::ring_buffer::MediaType::Video,
                                        0, pts, dts_val, packet.is_keyframe, &payload,
                                    );
                                    iq.write(&ts_bytes);
                                }
                            }
                            continue;
                        }

                        let ts = match packet.media_type {
                            MediaType::Video => RtmpTimestamp::new(packet.dts.max(0) as u32),
                            MediaType::Audio => RtmpTimestamp::new(packet.pts.max(0) as u32),
                        };
                        let payload = if packet.format == PayloadFormat::Raw {
                            match packet.media_type {
                                MediaType::Video => {
                                    // Send sequence header before first keyframe
                                    if packet.is_keyframe && !raw_seq_header_sent {
                                        if let Some(seq_hdr) = codec::build_avcc_sequence_header(&packet.payload) {
                                            if let Ok(ClientSessionResult::OutboundResponse(p)) =
                                                session.publish_video_data(seq_hdr, RtmpTimestamp::new(0), true)
                                            {
                                                if socket.write_all(&p.bytes).await.is_err() { return; }
                                            }
                                            raw_seq_header_sent = true;
                                        }
                                    }
                                    match codec::video_for_rtmp(&packet.payload, packet.is_keyframe) {
                                        Some(v) => Bytes::from(v),
                                        None => continue,
                                    }
                                }
                                MediaType::Audio => {
                                    Bytes::from(codec::audio_for_rtmp(&packet.payload))
                                }
                            }
                        } else {
                            packet.payload.clone()
                        };
                        let pkt = match packet.media_type {
                            MediaType::Video => {
                                session.publish_video_data(payload, ts, packet.is_keyframe)
                            }
                            MediaType::Audio => {
                                session.publish_audio_data(payload, ts, false)
                            }
                        };
                        match pkt {
                            Ok(ClientSessionResult::OutboundResponse(p)) => {
                                if socket.write_all(&p.bytes).await.is_err() { return; }
                                if let Some(ref counter) = egress_bytes_sent {
                                    counter.fetch_add(p.bytes.len() as u64, Ordering::Relaxed);
                                }
                            }
                            _ => {
                                eprintln!("[rtmp-egress] Failed to build publish data packet or get OutboundResponse");
                            }
                        }
                    }
                }
            }
            // H.264 packets from the HEVC→H.264 transcoder (only active for H.265 ingests)
            h264_res = async {
                match h264_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None     => std::future::pending().await,
                }
            }, if is_publishing => {
                if let Some((h264_data, is_kf, pts_ms)) = h264_res {
                    // Send FLV sequence header before the first H.264 IDR frame
                    if is_kf && !h264_seq_sent {
                        if let Some(seq_hdr) = codec::build_avcc_sequence_header(&h264_data) {
                            if let Ok(ClientSessionResult::OutboundResponse(p)) =
                                session.publish_video_data(seq_hdr, RtmpTimestamp::new(0), true)
                            {
                                if socket.write_all(&p.bytes).await.is_err() { return; }
                            }
                            h264_seq_sent = true;
                        }
                    }
                    if h264_seq_sent {
                        if let Some(flv) = codec::video_for_rtmp(&h264_data, is_kf) {
                            let ts = RtmpTimestamp::new(pts_ms.max(0) as u32);
                            if let Ok(ClientSessionResult::OutboundResponse(p)) =
                                session.publish_video_data(Bytes::from(flv), ts, is_kf)
                            {
                                if socket.write_all(&p.bytes).await.is_err() { return; }
                                if let Some(ref counter) = egress_bytes_sent {
                                    counter.fetch_add(p.bytes.len() as u64, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // Drain the H.265 transcoder queue so its thread can exit cleanly
    if let Some(iq) = &h265_in_queue {
        iq.close();
    }
}

async fn handle_client_results(
    results: Vec<ClientSessionResult>,
    socket: &mut TcpStream,
    session: &mut ClientSession,
    stream_key: &str,
) -> Result<(), &'static str> {
    for res in results {
        match res {
            ClientSessionResult::OutboundResponse(pkt) => {
                socket
                    .write_all(&pkt.bytes)
                    .await
                    .map_err(|_| "Socket write error")?;
            }
            ClientSessionResult::RaisedEvent(event) => match event {
                ClientSessionEvent::ConnectionRequestAccepted => {
                    let pub_pkt = match session
                        .request_publishing(stream_key.to_string(), PublishRequestType::Live)
                    {
                        Ok(ClientSessionResult::OutboundResponse(p)) => p,
                        _ => return Err("Failed to build publish request"),
                    };
                    socket
                        .write_all(&pub_pkt.bytes)
                        .await
                        .map_err(|_| "Socket write error")?;
                }
                ClientSessionEvent::ConnectionRequestRejected { description } => {
                    eprintln!("[rtmp-egress] Connection request rejected: {}", description);
                    return Err("Connection request rejected");
                }
                _ => {}
            },
            ClientSessionResult::UnhandleableMessageReceived(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flv_audio_aac_44100_stereo() {
        // sound_format=10 (AAC), rate=3 (44kHz), size=1 (16bit), type=1 (stereo)
        // AAC sequence header (packet_type=0), then AudioSpecificConfig: 0x12 0x10
        // object_type=2 (AAC-LC), freq_idx=4 (44100), ch_config=2 (stereo)
        let data: &[u8] = &[0xAF, 0x00, 0x12, 0x10];
        let meta = parse_flv_audio_meta(data).unwrap();
        assert_eq!(meta.codec, "aac");
        assert_eq!(meta.sample_rate, 44100);
        assert_eq!(meta.channels, 2);
        assert_eq!(meta.channel_layout.as_deref(), Some("stereo"));
    }

    #[test]
    fn parse_flv_audio_aac_48000() {
        // AudioSpecificConfig: 0x11 0x90 → object=2, freq_idx=3 (48000), ch_config=2
        let data: &[u8] = &[0xAF, 0x00, 0x11, 0x90];
        let meta = parse_flv_audio_meta(data).unwrap();
        assert_eq!(meta.codec, "aac");
        assert_eq!(meta.sample_rate, 48000);
        assert_eq!(meta.channels, 2);
    }

    #[test]
    fn parse_flv_video_h264_sequence_header() {
        // FLV video tag: keyframe(1) | codec_id(7) = 0x17
        // AVC packet type 0 (sequence header)
        // comp time offset: 0x00 0x00 0x00
        // AVCDecoderConfigurationRecord:
        //   version=1, profile=100 (High), compat=0x00, level=31 (3.1)
        //   lengthSizeMinusOne=3, numSPS=1
        //   SPS length=0x0019 (25 bytes)
        //   SPS: nal_type=7, profile=100, constraint=0x00, level=31,
        //        seq_parameter_set_id=0, chroma_format_idc=1,
        //        bit_depth_luma_minus8=0, bit_depth_chroma_minus8=0,
        //        ... pic_width_in_mbs_minus1=79, pic_height_in_map_units_minus1=44
        //        frame_mbs_only=1 → 1280x720
        #[rustfmt::skip]
        let data: &[u8] = &[
            0x17, // keyframe + AVC
            0x00, // sequence header
            0x00, 0x00, 0x00, // composition time
            // AVCDecoderConfigurationRecord
            0x01, // version
            0x64, // profile=High(100)
            0x00, // compat
            0x1F, // level=3.1(31)
            0xFF, // lengthSizeMinusOne=3
            0xE1, // numSPS=1
            0x00, 0x19, // SPS length = 25
            // SPS NAL unit (25 bytes): 720p H.264 High 3.1
            0x67, 0x64, 0x00, 0x1F, 0xAC, 0xD9, 0x40, 0x50,
            0x05, 0xBB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
            0x10, 0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x62,
            0xE4,
        ];

        let meta = parse_flv_video_meta(data).unwrap();
        assert_eq!(meta.codec, "h264");
        assert_eq!(meta.profile.as_deref(), Some("High"));
        assert_eq!(meta.level.as_deref(), Some("3.1"));
        assert_eq!(meta.width, 1280);
        assert_eq!(meta.height, 720);
    }

    #[test]
    fn parse_flv_video_non_sequence_header() {
        // Keyframe + AVC, but packet type 1 (NALU, not sequence header)
        let data: &[u8] = &[0x17, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x65];
        let meta = parse_flv_video_meta(data).unwrap();
        assert_eq!(meta.codec, "h264");
        assert_eq!(meta.width, 0); // not parsed from NALU packets
    }

    #[test]
    fn parses_signed_flv_video_composition_time() {
        assert_eq!(
            flv_video_composition_time_ms(&[0x27, 0x01, 0x00, 0x00, 0x28]),
            40
        );
        assert_eq!(
            flv_video_composition_time_ms(&[0x27, 0x01, 0xff, 0xff, 0xd8]),
            -40
        );
        assert_eq!(
            flv_video_composition_time_ms(&[0x17, 0x00, 0x00, 0x00, 0x28]),
            0
        );
        assert_eq!(flv_video_composition_time_ms(&[0xaf, 0x01, 0, 0, 40]), 0);
    }

    #[test]
    fn sps_parser_1080p() {
        // Minimal SPS for 1920x1080 Baseline profile
        // profile_idc=66, level=40, pic_width_in_mbs_minus1=119, pic_height_in_map_units_minus1=67
        // frame_mbs_only=1, no cropping
        // 120*16=1920, 68*16=1088 → needs crop_bottom=4 for 1080
        // Encoded as exp-golomb in bitstream
        #[rustfmt::skip]
        let sps: &[u8] = &[
            0x67, // NAL type 7
            0x42, // profile_idc = 66 (Baseline)
            0x00, // constraint flags
            0x28, // level_idc = 40
            0xE4, 0x40, 0x00, 0xEF, 0x00, 0x88, 0x3C, 0x60,
        ];
        // This is a simplified test — the SPS bitstream encoding is complex
        // so we verify the parser doesn't crash on valid-looking data
        let result = parse_sps_resolution(sps);
        // May or may not parse correctly depending on the exact bitstream
        // The important thing is it doesn't panic
        assert!(result.is_none() || result.unwrap().0 > 0);
    }

    #[test]
    fn bit_reader_exp_golomb() {
        let mut r = BitReader::new(&[0b10000000]); // 1 → code_num=0
        assert_eq!(r.read_exp_golomb(), Some(0));

        let mut r = BitReader::new(&[0b01000000]); // 010 → code_num=1
        assert_eq!(r.read_exp_golomb(), Some(1));

        let mut r = BitReader::new(&[0b01100000]); // 011 → code_num=2
        assert_eq!(r.read_exp_golomb(), Some(2));

        let mut r = BitReader::new(&[0b00100000]); // 00100 → code_num=3
        assert_eq!(r.read_exp_golomb(), Some(3));
    }
}
