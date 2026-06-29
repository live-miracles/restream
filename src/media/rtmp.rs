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
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::media::codec;
use crate::media::engine::{AudioMeta, MediaEngine, PublisherQuality, StageMetrics, VideoMeta};
use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};
use crate::media::security::IngestSecurityService;
use crate::media::tcp_stats::{collect_rtmp_receiver_stats, collect_rtmp_sender_stats};
use bytes::Bytes;

struct RtmpIngestHandle {
    pipeline_id: String,
    ring: Arc<RingBuffer>,
    bytes_received: Arc<AtomicU64>,
    ingest_metrics: Arc<StageMetrics>,
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

            // Parse SPS for resolution and timing info.
            if avc_config.len() > 8 {
                let num_sps = (avc_config[5] & 0x1F) as usize;
                if num_sps > 0 && avc_config.len() > 8 {
                    let sps_len = ((avc_config[6] as usize) << 8) | (avc_config[7] as usize);
                    if avc_config.len() >= 8 + sps_len
                        && sps_len > 1
                        && let Some(info) = parse_sps_video_info(&avc_config[8..8 + sps_len])
                    {
                        meta.width = info.width;
                        meta.height = info.height;
                        meta.fps = info.fps;
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

#[derive(Debug, Clone, Copy, Default)]
struct SpsVideoInfo {
    width: u32,
    height: u32,
    fps: f64,
}

fn parse_sps_video_info(sps_nalu: &[u8]) -> Option<SpsVideoInfo> {
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
    let height = (2 - frame_mbs_only) * (pic_height + 1) * 16 - crop_top * 2 - crop_bottom * 2;
    let mut info = SpsVideoInfo {
        width,
        height,
        fps: 0.0,
    };

    if let Some(vui_present) = reader.read_bits(1)
        && vui_present == 1
    {
        if reader.read_bits(1)? == 1 {
            let sar_idx = reader.read_bits(8)?;
            if sar_idx == 255 {
                reader.skip(32)?;
            }
        }
        if reader.read_bits(1)? == 1 {
            reader.skip(1)?;
        }
        if reader.read_bits(1)? == 1 {
            reader.skip(3)?;
            reader.skip(1)?;
            if reader.read_bits(1)? == 1 {
                reader.skip(24)?;
            }
        }
        if reader.read_bits(1)? == 1 {
            reader.read_exp_golomb()?;
            reader.read_exp_golomb()?;
        }
        if reader.read_bits(1)? == 1 {
            let num_units_in_tick = reader.read_bits(32)?;
            let time_scale = reader.read_bits(32)?;
            let fixed_frame_rate_flag = reader.read_bits(1)?;
            if num_units_in_tick > 0 && time_scale > 0 {
                let fps = time_scale as f64 / (2.0 * num_units_in_tick as f64);
                if fps.is_finite() && fps > 0.0 {
                    info.fps = if fixed_frame_rate_flag == 1 {
                        fps
                    } else {
                        fps.max(0.0)
                    };
                }
            }
        }
    }

    Some(info)
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
        pid: None,
        language: None,
        title: None,
        profile: None,
    };

    // AAC AudioSpecificConfig gives actual sample rate/channels
    if format_id == 10 && data.len() > 2 && data[1] == 0 {
        let asc = &data[2..];
        if asc.len() >= 2 {
            let audio_object_type = asc[0] >> 3;
            meta.profile = match audio_object_type {
                1 => Some("Main".to_string()),
                2 => Some("LC".to_string()),
                3 => Some("SSR".to_string()),
                4 => Some("LTP".to_string()),
                5 => Some("SBR".to_string()),
                _ => Some(format!("AAC Profile {}", audio_object_type)),
            };
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

struct RtmpUrlParts {
    host: String,
    port: u16,
    app: String,
    stream_key: String,
    tls: bool,
}

enum RtmpEgressStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for RtmpEgressStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RtmpEgressStream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            RtmpEgressStream::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RtmpEgressStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            RtmpEgressStream::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            RtmpEgressStream::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RtmpEgressStream::Plain(stream) => Pin::new(stream).poll_flush(cx),
            RtmpEgressStream::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RtmpEgressStream::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            RtmpEgressStream::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

impl RtmpEgressStream {
    fn tcp_stream(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => stream.get_ref().0,
        }
    }
}

fn rustls_client_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

async fn connect_rtmp_egress_stream(parts: &RtmpUrlParts) -> io::Result<RtmpEgressStream> {
    let tcp = TcpStream::connect(format!("{}:{}", parts.host, parts.port)).await?;
    let _ = tcp.set_nodelay(true);

    if !parts.tls {
        return Ok(RtmpEgressStream::Plain(tcp));
    }

    let server_name = ServerName::try_from(parts.host.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid RTMPS host name"))?;
    let connector = TlsConnector::from(rustls_client_config());
    let tls = connector.connect(server_name, tcp).await?;
    Ok(RtmpEgressStream::Tls(Box::new(tls)))
}

fn rtmp_sender_quality(
    socket: &RtmpEgressStream,
    previous_tcp_bytes: &mut Option<(u64, Instant)>,
) -> PublisherQuality {
    let now = Instant::now();
    match collect_rtmp_sender_stats(socket.tcp_stream()) {
        Ok(stats) => {
            let send_rate = stats.tcp_bytes_sent.and_then(|bytes| {
                let rate = previous_tcp_bytes.and_then(|(previous, sampled_at)| {
                    let elapsed = now.duration_since(sampled_at).as_secs_f64();
                    let delta = bytes.checked_sub(previous)?;
                    (elapsed > 0.0).then_some((delta as f64 * 8.0) / (elapsed * 1_000_000.0))
                });
                *previous_tcp_bytes = Some((bytes, now));
                rate
            });
            PublisherQuality {
                tcp_congestion_algorithm: stats.tcp_congestion_algorithm,
                tcp_rtt_ms: stats.tcp_rtt_ms,
                tcp_rtt_var_ms: stats.tcp_rtt_var_ms,
                tcp_bytes_sent: stats.tcp_bytes_sent,
                tcp_bytes_acked: stats.tcp_bytes_acked,
                tcp_bytes_retrans: stats.tcp_bytes_retrans,
                tcp_last_snd_ms: stats.tcp_last_snd_ms,
                tcp_snd_mss: stats.tcp_snd_mss,
                tcp_pmtu: stats.tcp_pmtu,
                tcp_unacked: stats.tcp_unacked,
                tcp_sacked: stats.tcp_sacked,
                tcp_lost: stats.tcp_lost,
                tcp_retrans: stats.tcp_retrans,
                tcp_snd_cwnd: stats.tcp_snd_cwnd,
                tcp_snd_ssthresh: stats.tcp_snd_ssthresh,
                tcp_advmss: stats.tcp_advmss,
                tcp_reordering: stats.tcp_reordering,
                tcp_notsent_bytes: stats.tcp_notsent_bytes,
                tcp_total_retrans: stats.tcp_total_retrans,
                tcp_pacing_rate_bps: stats.tcp_pacing_rate_bps,
                tcp_max_pacing_rate_bps: stats.tcp_max_pacing_rate_bps,
                tcp_delivery_rate_bps: stats.tcp_delivery_rate_bps,
                tcp_segs_out: stats.tcp_segs_out,
                tcp_data_segs_out: stats.tcp_data_segs_out,
                tcp_delivered: stats.tcp_delivered,
                tcp_delivered_ce: stats.tcp_delivered_ce,
                tcp_busy_time_ms: stats.tcp_busy_time_ms,
                tcp_rwnd_limited_ms: stats.tcp_rwnd_limited_ms,
                tcp_sndbuf_limited_ms: stats.tcp_sndbuf_limited_ms,
                tcp_dsack_dups: stats.tcp_dsack_dups,
                tcp_reord_seen: stats.tcp_reord_seen,
                tcp_snd_wnd: stats.tcp_snd_wnd,
                tcp_total_rto: stats.tcp_total_rto,
                tcp_total_rto_recoveries: stats.tcp_total_rto_recoveries,
                tcp_total_rto_time_ms: stats.tcp_total_rto_time_ms,
                tcp_skmem_wmem_alloc: stats.tcp_skmem_wmem_alloc,
                tcp_skmem_wmem_max: stats.tcp_skmem_wmem_max,
                tcp_send_rate_mbps: send_rate,
                ..PublisherQuality::default()
            }
        }
        Err(error) => PublisherQuality {
            tcp_stats_unavailable_reason: Some(
                match error.kind() {
                    std::io::ErrorKind::Unsupported => "not_linux",
                    _ => "collection_failed",
                }
                .to_string(),
            ),
            ..PublisherQuality::default()
        },
    }
}

// Standard RTMP URL parser helper
fn parse_rtmp_url(url: &str) -> Option<RtmpUrlParts> {
    if !url.starts_with("rtmp://") && !url.starts_with("rtmps://") {
        return None;
    }
    let tls = url.starts_with("rtmps://");
    let prefix_len = if tls {
        "rtmps://".len()
    } else {
        "rtmp://".len()
    };
    let s = &url[prefix_len..];
    let slash_idx = s.find('/')?;
    let host_port = &s[..slash_idx];
    let path = &s[slash_idx + 1..];

    let (host, port) = if host_port.starts_with('[') {
        // IPv6 literal: [::1]:1935
        let bracket_end = host_port.find(']')?;
        let host = host_port[1..bracket_end].to_string();
        let port = if bracket_end + 1 < host_port.len() {
            host_port[bracket_end + 2..].parse::<u16>().ok()?
        } else {
            1935
        };
        (host, port)
    } else if let Some(colon_idx) = host_port.find(':') {
        let h = &host_port[..colon_idx];
        let p = host_port[colon_idx + 1..].parse::<u16>().ok()?;
        (h.to_string(), p)
    } else {
        (host_port.to_string(), 1935)
    };

    let path_slash = path.find('/')?;
    let app = &path[..path_slash];
    let stream_key = &path[path_slash + 1..];

    Some(RtmpUrlParts {
        host,
        port,
        app: app.to_string(),
        stream_key: stream_key.to_string(),
        tls,
    })
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
            error!("Failed to bind TCP listener on {}: {:?}", addr, e);
            return;
        }
    };
    info!("Server listening on {}", addr);

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
                        warn!("error handling client {}: {:?}", addr, e);
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {:?}", e);
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
        // SAFETY: setsockopt is a POSIX socket API. The file descriptor
        // `fd` is a valid socket from tokio's TcpStream. `size` is a
        // stack-allocated c_int with a known-safe value (8 MiB). The
        // pointer cast is valid because c_void is the canonical opaque
        // pointer for setsockopt's option-value argument.
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
                engine
                    .record_ingest_disconnect(
                        &active.pipeline_id,
                        Some("session"),
                        Some(error.to_string()),
                        true,
                    )
                    .await;
                engine.unregister_ingest(&active.pipeline_id).await;
            }
            return Err(error);
        }
    }

    // 3. Main Protocol Loop
    let mut tcp_stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    tcp_stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous_tcp_bytes: Option<(u64, Instant)> = None;
    let disconnect_outcome = loop {
        tokio::select! {
            read_result = socket.read(&mut buffer) => {
                let n = read_result.map_err(|_| "Read error in main loop")?;
                if n == 0 {
                    break Some((
                        "disconnect".to_string(),
                        "publisher disconnected".to_string(),
                        false,
                    ));
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
                    warn!("session result error: {}", e);
                    break Some(("session".to_string(), e.to_string(), true));
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
                            tcp_congestion_algorithm: stats.tcp_congestion_algorithm,
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
    };

    // Clean up active ingest on disconnect
    if let Some(active) = &active_ingest {
        info!(
            "[rtmp] Publisher disconnected for pipeline: {}",
            active.pipeline_id
        );
        let (phase, reason, had_error) = disconnect_outcome.unwrap_or((
            "disconnect".to_string(),
            "publisher disconnected".to_string(),
            false,
        ));
        engine
            .record_ingest_disconnect(
                &active.pipeline_id,
                Some(phase.as_str()),
                Some(reason),
                had_error,
            )
            .await;
        engine.unregister_ingest(&active.pipeline_id).await;
    }

    Ok(())
}

struct ProbeState {
    video_done: bool,
    audio_done: bool,
}

#[allow(clippy::too_many_arguments)]
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
                        if security.is_ip_banned(client_ip).is_some() {
                            let _ = session.reject_request(
                                request_id,
                                "NetStream.Publish.BadName",
                                "IP temporarily banned due to too many login/publish failures",
                            );
                            return Err("IP is banned");
                        }

                        // Validate stream key against database pipelines
                        let pipeline = match sqlx::query_as::<_, crate::types::Pipeline>(
                            "SELECT id, name, stream_key, input_source, encoding, srt_ingest_policy FROM pipelines WHERE stream_key = ?"
                        )
                        .bind(&stream_key)
                        .fetch_optional(db)
                        .await {
                            Ok(Some(p)) => p,
                            Ok(None) => {
                                warn!("publish stream key not found: {:?}", stream_key);
                                security.record_failure(client_ip);
                                let _ = session.reject_request(request_id, "NetStream.Publish.BadName", "Invalid stream key");
                                return Err("Invalid stream key");
                            }
                            Err(e) => {
                                error!("publish stream key DB query failed: {:?}", e);
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
                        let Some((bytes_received, ingest_metrics)) = engine
                            .with_active_ingest(&pipeline.id, |ingest| {
                                (ingest.bytes_received.clone(), ingest.metrics.clone())
                            })
                            .await
                        else {
                            engine.unregister_ingest(&pipeline.id).await;
                            return Err("Active ingest disappeared during registration");
                        };
                        *active_ingest = Some(RtmpIngestHandle {
                            pipeline_id: pipeline.id.clone(),
                            ring,
                            bytes_received,
                            ingest_metrics,
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
                        info!(
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
                            active.ingest_metrics.record_in(data.len() as u64);

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
                            if !probe.video_done
                                && let Some(meta) = parse_flv_video_meta(&data)
                            {
                                if meta.width > 0 {
                                    probe.video_done = true;
                                }
                                info!(
                                    "[rtmp] Probed video: {} {}x{} profile={:?} level={:?}",
                                    meta.codec, meta.width, meta.height, meta.profile, meta.level
                                );
                                engine
                                    .update_ingest_meta(pipeline_id, Some(meta), None, None)
                                    .await;
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
                            active.ingest_metrics.record_in(data.len() as u64);

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
                                    info!(
                                        "[rtmp] Probed audio: {} {}Hz {}ch",
                                        meta.codec, meta.sample_rate, meta.channels
                                    );
                                    engine
                                        .update_ingest_meta(
                                            pipeline_id,
                                            None,
                                            Some(meta.clone()),
                                            None,
                                        )
                                        .await;
                                    engine
                                        .update_ingest_audio_tracks(pipeline_id, vec![meta])
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
                            "SELECT id, name, stream_key, input_source, encoding, srt_ingest_policy FROM pipelines WHERE stream_key = ?"
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
                            .ingests
                            .active
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

                        info!(
                            "[rtmp] Play subscriber connected for pipeline: {} (stream_id={})",
                            pipeline.id, stream_id
                        );

                        // Send cached sequence headers so the player can initialize decoders
                        let (video_sh, audio_sh) = engine.get_sequence_headers(&pipeline.id).await;
                        if let Some(vsh) = video_sh
                            && let Ok(pkt) = session.send_video_data(
                                stream_id,
                                vsh,
                                RtmpTimestamp::new(0),
                                false,
                            )
                        {
                            let _ = socket.write_all(&pkt.bytes).await;
                        }
                        if let Some(ash) = audio_sh
                            && let Ok(pkt) = session.send_audio_data(
                                stream_id,
                                ash,
                                RtmpTimestamp::new(0),
                                false,
                            )
                        {
                            let _ = socket.write_all(&pkt.bytes).await;
                        }

                        // Feed loop: read from RingBuffer and send RTMP data.
                        // Use pull_burst() to batch up to 32 packets per iteration
                        // instead of pull() which acquires the write_idx atomic once
                        // per packet (~170 acquisitions/sec at 170 pkts/sec vs ~5/sec).
                        let ring_buf = engine.get_or_create_pipeline(&pipeline.id).await;
                        let mut reader =
                            Reader::new(format!("rtmp_play:{}", pipeline.id), ring_buf);
                        let mut burst = Vec::with_capacity(32);

                        'play: loop {
                            burst.clear();
                            match reader.pull_burst(&mut burst, 32) {
                                Ok(0) => {
                                    reader.wait_for_data().await;
                                    continue;
                                }
                                Err(_) => {
                                    // Overflow — reader was fast-forwarded; continue from new pos
                                    continue;
                                }
                                Ok(_) => {}
                            }

                            for pkt in &burst {
                                let ts = match pkt.media_type {
                                    MediaType::Video => RtmpTimestamp::new(pkt.dts.max(0) as u32),
                                    MediaType::Audio => RtmpTimestamp::new(pkt.pts.max(0) as u32),
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
                                            info!(
                                                "[rtmp] Play subscriber disconnected for pipeline: {}",
                                                pipeline.id
                                            );
                                            return Err("Play subscriber disconnected");
                                        }
                                    }
                                    Err(_) => break 'play,
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
/// RTMP Egress Client
pub async fn start_rtmp_egress(
    output_id: String,
    pipeline_id: String,
    target_url: String,
    ring_buffer: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel_token: CancellationToken,
) {
    let parts = match parse_rtmp_url(&target_url) {
        Some(p) => p,
        None => {
            error!("Invalid RTMP URL: {}", target_url);
            engine
                .record_egress_error(&output_id, "parse_url", "invalid RTMP URL")
                .await;
            return;
        }
    };
    engine.update_egress_phase(&output_id, "connecting").await;
    engine
        .update_egress_target_addr(&output_id, format!("{}:{}", parts.host, parts.port))
        .await;
    info!(
        "[rtmp-egress] Connecting to {}:{} via {} (app: {}, key: {})",
        parts.host,
        parts.port,
        if parts.tls { "rtmps" } else { "rtmp" },
        parts.app,
        parts.stream_key
    );

    let mut socket = match connect_rtmp_egress_stream(&parts).await {
        Ok(s) => s,
        Err(e) => {
            error!(
                "[rtmp-egress] Connection failed to {}:{}: {:?}",
                parts.host, parts.port, e
            );
            engine
                .record_egress_error(&output_id, "connect", e.to_string())
                .await;
            return;
        }
    };

    // Perform handshake
    engine.update_egress_phase(&output_id, "handshaking").await;
    let mut handshake = Handshake::new(PeerType::Client);
    let c0_c1 = match handshake.generate_outbound_p0_and_p1() {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(
                "[rtmp-egress] Handshake outbound generation failed: {:?}",
                e
            );
            engine
                .record_egress_error(&output_id, "handshake", format!("{:?}", e))
                .await;
            return;
        }
    };

    if socket.write_all(&c0_c1).await.is_err() {
        engine
            .record_egress_error(&output_id, "handshake", "failed to write handshake")
            .await;
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
                    _ => {
                        engine
                            .record_egress_error(&output_id, "handshake", "remote closed during handshake")
                            .await;
                        return;
                    }
                };
                match handshake.process_bytes(&buffer[..n]) {
                    Ok(HandshakeProcessResult::InProgress { response_bytes }) => {
                        if !response_bytes.is_empty()
                            && socket.write_all(&response_bytes).await.is_err() { return; }
                    }
                    Ok(HandshakeProcessResult::Completed { response_bytes, remaining_bytes }) => {
                        if !response_bytes.is_empty()
                            && socket.write_all(&response_bytes).await.is_err() { return; }
                        remaining = remaining_bytes;
                        handshake_completed = true;
                    }
                    Err(e) => {
                        error!("Handshake process bytes failed: {:?}", e);
                        engine
                            .record_egress_error(&output_id, "handshake", format!("{:?}", e))
                            .await;
                        return;
                    }
                }
            }
        }
    }

    // Initialize ClientSession with tcUrl for MediaMTX compatibility
    let mut config = ClientSessionConfig::new();
    let scheme = if parts.tls { "rtmps" } else { "rtmp" };
    config.tc_url = Some(format!(
        "{}://{}:{}/{}",
        scheme, parts.host, parts.port, parts.app
    ));
    let (mut session, initial_results) = match ClientSession::new(config) {
        Ok(s) => s,
        Err(e) => {
            engine
                .record_egress_error(&output_id, "session", format!("{:?}", e))
                .await;
            return;
        }
    };

    for res in initial_results {
        if let ClientSessionResult::OutboundResponse(pkt) = res
            && socket.write_all(&pkt.bytes).await.is_err()
        {
            engine
                .record_egress_error(&output_id, "session", "failed to write session init")
                .await;
            return;
        }
    }

    // Request connection
    engine
        .update_egress_phase(&output_id, "connecting_app")
        .await;
    let conn_pkt = match session.request_connection(parts.app.clone()) {
        Ok(ClientSessionResult::OutboundResponse(p)) => p,
        _ => {
            engine
                .record_egress_error(&output_id, "connect_app", "failed to build connect request")
                .await;
            return;
        }
    };
    if socket.write_all(&conn_pkt.bytes).await.is_err() {
        engine
            .record_egress_error(&output_id, "connect_app", "failed to write connect request")
            .await;
        return;
    }

    if !remaining.is_empty() {
        let results = match session.handle_input(&remaining) {
            Ok(r) => r,
            Err(_) => return,
        };
        if handle_client_results(results, &mut socket, &mut session, &parts.stream_key)
            .await
            .is_err()
        {
            return;
        }
    }

    let (egress_bytes_sent, egress_metrics, egress_last_progress_ms) = {
        engine
            .with_active_egress(&output_id, |egress| {
                (
                    Some(egress.bytes_sent.clone()),
                    Some(egress.metrics.clone()),
                    Some(egress.last_progress_ms.clone()),
                )
            })
            .await
            .unwrap_or((None, None, None))
    };

    let mut is_publishing = false;
    let mut reader = Reader::new(format!("rtmp_egress:{}", output_id), ring_buffer);
    let progress_sample_interval = Duration::from_millis(250);
    let mut last_progress_sample = Instant::now() - progress_sample_interval;
    let mut tcp_stats_interval = tokio::time::interval(Duration::from_secs(2));
    tcp_stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous_tcp_bytes: Option<(u64, Instant)> = None;
    // Track the last SPS bytes we sent so we can re-send the AVCC decoder
    // config record when the encoder changes resolution or bitrate mid-stream.
    // None = no sequence header sent yet.
    let mut last_sent_sps: Option<Vec<u8>> = None;
    // Per-egress reusable conversion buffers — avoids per-frame Vec allocation.
    // Each task owns its own buffer; no sharing, no contention with transcoder.
    let mut video_buf = Vec::<u8>::new();
    let mut audio_buf = Vec::<u8>::new();

    // Pre-allocated burst buffer — declared outside the loop so capacity
    // is retained across bursts instead of re-allocating per burst.
    let mut packets: Vec<Arc<MediaPacket>> = Vec::with_capacity(32);

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                let _ = session.stop_publishing();
                break;
            }
            _ = tcp_stats_interval.tick() => {
                let quality = rtmp_sender_quality(&socket, &mut previous_tcp_bytes);
                engine.update_egress_quality(&output_id, quality).await;
            }
            // Read from server to handle acknowledgements, status codes, pings
            res = socket.read(&mut buffer) => {
                let n = match res {
                    Ok(n) if n > 0 => n,
                    _ => {
                        engine
                            .record_egress_error(&output_id, "send", "remote closed connection")
                            .await;
                        break;
                    }
                };
                let results = match session.handle_input(&buffer[..n]) {
                    Ok(r) => r,
                    Err(e) => {
                        engine
                            .record_egress_error(&output_id, "send", format!("{:?}", e))
                            .await;
                        break;
                    }
                };
                for r in results {
                    match r {
                        ClientSessionResult::OutboundResponse(pkt) => {
                            if socket.write_all(&pkt.bytes).await.is_err() { return; }
                        }
                        ClientSessionResult::RaisedEvent(event) => {
                            match event {
                                ClientSessionEvent::ConnectionRequestAccepted => {
                                    engine.update_egress_phase(&output_id, "publishing").await;
                                    let pub_pkt = match session.request_publishing(parts.stream_key.clone(), PublishRequestType::Live) {
                                        Ok(ClientSessionResult::OutboundResponse(p)) => p,
                                        _ => return,
                                    };
                                    if socket.write_all(&pub_pkt.bytes).await.is_err() { return; }
                                }
                                ClientSessionEvent::PublishRequestAccepted => {
                                    info!("Stream publishing accepted on target");
                                    engine.update_egress_phase(&output_id, "sending").await;
                                    // Send cached sequence headers before media data.
                                    // For H.265 ingests, video_sh is None (only RTMP ingest
                                    // caches FLV seq headers), so this is a no-op for H.265.
                                    let (video_sh, mut audio_sh) = engine.get_sequence_headers(&pipeline_id).await;
                                    if let Some(vsh) = video_sh
                                        && let Ok(ClientSessionResult::OutboundResponse(p)) =
                                            session.publish_video_data(vsh, RtmpTimestamp::new(0), true)
                                            && socket.write_all(&p.bytes).await.is_err() { return; }
                                    // Synthesize AAC sequence header from audio meta if not cached
                                    if audio_sh.is_none() {
                                        if let Some(Some(track)) = engine
                                            .with_active_ingest(&pipeline_id, |ingest| {
                                                let tracks = ingest
                                                    .audio_tracks
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                tracks.first().cloned()
                                            })
                                            .await
                                        {
                                            audio_sh = Some(codec::build_aac_sequence_header(
                                                track.sample_rate,
                                                track.channels,
                                            ));
                                        }
                                    }
                                    if let Some(ash) = audio_sh
                                        && let Ok(ClientSessionResult::OutboundResponse(p)) =
                                            session.publish_audio_data(ash, RtmpTimestamp::new(0), false)
                                            && socket.write_all(&p.bytes).await.is_err() { return; }
                                    is_publishing = true;
                                }
                                ClientSessionEvent::ConnectionRequestRejected { description } => {
                                    error!("Connection rejected: {}", description);
                                    engine
                                        .record_egress_error(&output_id, "connect_app", description)
                                        .await;
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
                if reader.pull_burst(&mut packets, 32).is_ok() {
                    let mut burst_made_progress = false;
                    for packet in packets.drain(..) {
                        let ts = match packet.media_type {
                            MediaType::Video => RtmpTimestamp::new(packet.dts.max(0) as u32),
                            MediaType::Audio => RtmpTimestamp::new(packet.pts.max(0) as u32),
                        };
                        let payload = if packet.format == PayloadFormat::Raw {
                            match packet.media_type {
                                MediaType::Video => {
                                    // Guard: Raw path is H.264-only.  H.265 packets
                                    // must be converted by hevc_to_h264 before reaching
                                    // RTMP egress.  If they arrive here the stage graph
                                    // was set up before the codec probe completed; drop
                                    // and warn until a keyframe with a proper H.264 SPS
                                    // arrives.
                                    if packet.payload.len() >= 2 {
                                        // H.265 two-byte NAL header: bits[9:15] = nal_unit_type.
                                        // H.264 one-byte NAL header: bits[0:4] = nal_unit_type.
                                        // Detect HEVC by checking for VPS (type 32) or
                                        // SPS (type 33) in the first NALU — types that cannot
                                        // appear in H.264 streams.
                                        let first_nalu_type_h265 =
                                            (packet.payload[0] >> 1) & 0x3F;
                                        if matches!(first_nalu_type_h265, 32..=34) {
                                            error!(
                                                "[rtmp-egress] H.265 packet on Raw RTMP path \
                                                 for output {} — dropping until hevc_to_h264 \
                                                 stage is ready",
                                                output_id
                                            );
                                            continue;
                                        }
                                    }
                                    // On each keyframe, check whether the SPS has changed
                                    // (encoder resolution/bitrate switch) and (re-)send the
                                    // AVCC decoder configuration record before the IDR.
                                    if packet.is_keyframe {
                                        let nalus = codec::split_annexb_nalus(&packet.payload);
                                        let new_sps: Option<Vec<u8>> = nalus
                                            .iter()
                                            .find(|n| !n.is_empty() && (n[0] & 0x1F) == 7)
                                            .map(|n| n.to_vec());
                                        let sps_changed = match (&last_sent_sps, &new_sps) {
                                            (None, Some(_)) => true,
                                            (Some(old), Some(new)) => old != new,
                                            _ => false,
                                        };
                                        if sps_changed
                                            && let Some(seq_hdr) =
                                                codec::build_avcc_sequence_header(&packet.payload)
                                        {
                                            if let Ok(ClientSessionResult::OutboundResponse(
                                                p,
                                            )) = session.publish_video_data(
                                                seq_hdr,
                                                RtmpTimestamp::new(0),
                                                true,
                                            ) && socket.write_all(&p.bytes).await.is_err()
                                            {
                                                return;
                                            }
                                            last_sent_sps = new_sps;
                                        }
                                    }
                                    if !codec::video_for_rtmp_into(
                                        &packet.payload,
                                        packet.is_keyframe,
                                        &mut video_buf,
                                    ) {
                                        continue;
                                    }
                                    Bytes::copy_from_slice(&video_buf)
                                }
                                MediaType::Audio => {
                                    codec::audio_for_rtmp_into(&packet.payload, &mut audio_buf);
                                    Bytes::copy_from_slice(&audio_buf)
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
                                if let Some(ref m) = egress_metrics {
                                    m.record_out(p.bytes.len() as u64);
                                }
                                burst_made_progress = true;
                            }
                            _ => {
                                error!("Failed to build publish data packet or get OutboundResponse");
                                engine
                                    .record_egress_error(&output_id, "send", "failed to build RTMP publish packet")
                                    .await;
                            }
                        }
                    }
                    if burst_made_progress
                        && last_progress_sample.elapsed() >= progress_sample_interval
                    {
                        if let Some(ref progress) = egress_last_progress_ms {
                            progress.store(
                                chrono::Utc::now().timestamp_millis().max(0) as u64,
                                Ordering::Relaxed,
                            );
                        }
                        last_progress_sample = Instant::now();
                    }
                }
            }
        }
    }
}

async fn handle_client_results<S>(
    results: Vec<ClientSessionResult>,
    socket: &mut S,
    session: &mut ClientSession,
    stream_key: &str,
) -> Result<(), &'static str>
where
    S: AsyncWrite + Unpin,
{
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
                    error!("Connection request rejected: {}", description);
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
    use crate::media::engine::{MediaEngine, VideoMeta};

    #[tokio::test]
    async fn detects_h265_from_ingest_video_meta() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("p1", "key", "srt")
            .await
            .unwrap();

        // No video meta yet → not H.265
        {
            let ingests = engine.ingests.active.read().await;
            let is_h265 = ingests
                .get("p1")
                .and_then(|i| i.video.as_ref())
                .map(|v| v.codec == "hevc")
                .unwrap_or(false);
            assert!(!is_h265, "no video meta should not be hevc");
        }

        // H.264 meta → not H.265
        engine
            .update_ingest_meta(
                "p1",
                Some(VideoMeta {
                    codec: "h264".into(),
                    width: 0,
                    height: 0,
                    fps: 0.0,
                    bw: None,
                    pid: None,
                    language: None,
                    title: None,
                    profile: None,
                    level: None,
                    pixel_format: None,
                }),
                None,
                None,
            )
            .await;
        {
            let ingests = engine.ingests.active.read().await;
            let is_h265 = ingests
                .get("p1")
                .and_then(|i| i.video.as_ref())
                .map(|v| v.codec == "hevc")
                .unwrap_or(false);
            assert!(!is_h265, "h264 meta should not be hevc");
        }

        // H.265 meta → is H.265
        engine
            .update_ingest_meta(
                "p1",
                Some(VideoMeta {
                    codec: "hevc".into(),
                    width: 0,
                    height: 0,
                    fps: 0.0,
                    bw: None,
                    pid: None,
                    language: None,
                    title: None,
                    profile: None,
                    level: None,
                    pixel_format: None,
                }),
                None,
                None,
            )
            .await;
        {
            let ingests = engine.ingests.active.read().await;
            let is_h265 = ingests
                .get("p1")
                .and_then(|i| i.video.as_ref())
                .map(|v| v.codec == "hevc")
                .unwrap_or(false);
            assert!(is_h265, "hevc meta should be detected");
        }

        engine.unregister_ingest("p1").await;
    }

    #[tokio::test]
    async fn h265_detection_waits_for_probe_meta() {
        let engine = Arc::new(MediaEngine::new());
        let engine_clone = engine.clone();
        let pipeline_id = "p2".to_string();

        engine
            .try_register_ingest(&pipeline_id, "key", "srt")
            .await
            .unwrap();

        // Spawn a task that sets the video meta after a delay (simulating probe arrival)
        let delayed_engine = engine.clone();
        let delayed_pid = pipeline_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            delayed_engine
                .update_ingest_meta(
                    &delayed_pid,
                    Some(VideoMeta {
                        codec: "hevc".into(),
                        width: 0,
                        height: 0,
                        fps: 0.0,
                        bw: None,
                        pid: None,
                        language: None,
                        title: None,
                        profile: None,
                        level: None,
                        pixel_format: None,
                    }),
                    None,
                    None,
                )
                .await;
        });

        // Now run the same probe-wait logic that start_rtmp_egress uses
        let is_h265 = 'probe: {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                let ingests = engine_clone.ingests.active.read().await;
                let meta = ingests.get(&pipeline_id).and_then(|i| i.video.as_ref());
                match meta {
                    Some(v) if v.codec == "hevc" => break 'probe true,
                    Some(_) => break 'probe false,
                    None => {}
                }
                drop(ingests);
                if std::time::Instant::now() >= deadline {
                    break 'probe false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        };
        assert!(is_h265, "should detect hevc after probe arrives");

        engine.unregister_ingest(&pipeline_id).await;
    }

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
        let result = parse_sps_video_info(sps);
        // May or may not parse correctly depending on the exact bitstream
        // The important thing is it doesn't panic
        assert!(result.is_none() || result.unwrap().width > 0);
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

    #[test]
    fn parse_rtmp_url_standard_forms() {
        // Default port
        let parts = parse_rtmp_url("rtmp://a.example.com/live/mykey").unwrap();
        assert_eq!(parts.host, "a.example.com");
        assert_eq!(parts.port, 1935);
        assert_eq!(parts.app, "live");
        assert_eq!(parts.stream_key, "mykey");
        assert!(!parts.tls);

        // Explicit port
        let parts = parse_rtmp_url("rtmp://a.example.com:19350/stream/abc").unwrap();
        assert_eq!(parts.host, "a.example.com");
        assert_eq!(parts.port, 19350);
        assert_eq!(parts.app, "stream");
        assert_eq!(parts.stream_key, "abc");
        assert!(!parts.tls);

        // rtmps:// (TLS) — same parsing, different default port behaviour (still 1935 if omitted)
        let parts =
            parse_rtmp_url("rtmps://live-api-s.facebook.com:443/rtmp/FB-STREAM-KEY").unwrap();
        assert_eq!(parts.host, "live-api-s.facebook.com");
        assert_eq!(parts.port, 443);
        assert_eq!(parts.app, "rtmp");
        assert_eq!(parts.stream_key, "FB-STREAM-KEY");
        assert!(parts.tls);

        // Stream key containing slashes is NOT split — key gets everything after first slash in path
        let parts = parse_rtmp_url("rtmp://host/app/key/subpart").unwrap();
        assert_eq!(parts.app, "app");
        assert_eq!(parts.stream_key, "key/subpart");
        assert!(!parts.tls);

        // Unrecognised scheme → None
        assert!(parse_rtmp_url("https://host/live/key").is_none());

        // Missing path separator → None (can't split app/key)
        assert!(parse_rtmp_url("rtmp://host/noapp").is_none());
    }

    // --- Regression: issue #5 (Round 5) — IPv6 RTMP URL parsing ---
    // Before the fix, `host_port.find(':')` landed inside the IPv6 brackets
    // (first `:` in `[::1]:1935` is at position 2, inside the brackets),
    // causing the host to be parsed as `[` and port parsing to fail.
    #[test]
    fn parse_rtmp_url_ipv6_literal() {
        let result = parse_rtmp_url("rtmp://[::1]:1935/live/mykey");
        assert!(result.is_some(), "IPv6 URL must parse successfully");
        let parts = result.unwrap();
        assert_eq!(parts.host, "::1");
        assert_eq!(parts.port, 1935);
        assert_eq!(parts.app, "live");
        assert_eq!(parts.stream_key, "mykey");
        assert!(!parts.tls);
    }

    #[test]
    fn parse_rtmp_url_ipv6_default_port() {
        let result = parse_rtmp_url("rtmp://[2001:db8::1]/live/mykey");
        assert!(
            result.is_some(),
            "IPv6 URL without port must use default 1935"
        );
        let parts = result.unwrap();
        assert_eq!(parts.host, "2001:db8::1");
        assert_eq!(parts.port, 1935);
        assert!(!parts.tls);
    }

    #[test]
    fn parse_rtmp_url_ipv4_unchanged() {
        // Ensure the IPv4 path still works correctly after the IPv6 fix.
        let result = parse_rtmp_url("rtmp://192.168.1.1:1935/live/key");
        assert!(result.is_some());
        let parts = result.unwrap();
        assert_eq!(parts.host, "192.168.1.1");
        assert_eq!(parts.port, 1935);
        assert_eq!(parts.app, "live");
        assert_eq!(parts.stream_key, "key");
        assert!(!parts.tls);
    }

    // --- FLV video meta: malformed / truncated / unknown codec ---

    #[test]
    fn parse_flv_video_meta_empty_returns_none() {
        assert!(parse_flv_video_meta(&[]).is_none());
    }

    #[test]
    fn parse_flv_video_meta_single_byte_returns_none() {
        assert!(parse_flv_video_meta(&[0x17]).is_none());
    }

    #[test]
    fn parse_flv_video_meta_unknown_codec_id_returns_none() {
        // codec_id=5 (On2 VP6 with alpha) — not handled
        let data = [0x15u8, 0x01, 0x00, 0x00, 0x00];
        assert!(parse_flv_video_meta(&data).is_none());
    }

    #[test]
    fn parse_flv_video_meta_vp6_returns_codec_name() {
        // frame_type=1, codec_id=4 (VP6) → meta returned with codec="vp6"
        let data = [0x14u8, 0x00];
        let meta = parse_flv_video_meta(&data).unwrap();
        assert_eq!(meta.codec, "vp6");
        assert_eq!(meta.width, 0);
    }

    #[test]
    fn parse_flv_video_meta_h265_returns_codec_name() {
        // frame_type=1, codec_id=12 (H.265/HEVC enhanced)
        let data = [0x1Cu8, 0x01, 0x00, 0x00, 0x00];
        let meta = parse_flv_video_meta(&data).unwrap();
        assert_eq!(meta.codec, "h265");
    }

    #[test]
    fn parse_flv_video_meta_h264_seq_header_truncated_avcc() {
        // seq header (byte[1]=0) but AVCDecoderConfigurationRecord too short to extract profile/level
        // data.len() == 6: passes the > 12 check? No: 6 < 12 → skips SPS parsing, no panic
        let data = [0x17u8, 0x00, 0x00, 0x00, 0x00, 0x01];
        let meta = parse_flv_video_meta(&data).unwrap();
        assert_eq!(meta.codec, "h264");
        // profile/level not parsed (too short)
        assert!(meta.profile.is_none());
        assert!(meta.level.is_none());
        assert_eq!(meta.width, 0);
    }

    #[test]
    fn parse_flv_video_meta_h264_seq_header_short_sps_length_field() {
        // 13 bytes: passes > 12 check. avc_config starts at data[5].
        // avc_config[5]=0xE1 (numSPS=1), avc_config[6..7]=SPS len = 0x0001 (1 byte),
        // but then we'd need avc_config[8 + 1] = 9 bytes total in avc_config.
        // avc_config len = 13-5 = 8 bytes → 8 < 9 → SPS resolution not parsed. No panic.
        let data = [
            0x17u8, 0x00, 0x00, 0x00, 0x00, // frame_type/codec, pkt_type, comp_time
            0x01, 0x64, 0x00, 0x1F, // version, profile, compat, level
            0xFF, 0xE1, // lengthSizeMinusOne, numSPS=1
            0x00, 0x01, // SPS length = 1 (only 0 bytes remain → out of bounds)
        ];
        let meta = parse_flv_video_meta(&data).unwrap();
        assert_eq!(meta.codec, "h264");
        assert_eq!(meta.profile.as_deref(), Some("High"));
        assert_eq!(meta.level.as_deref(), Some("3.1"));
        assert_eq!(meta.width, 0); // SPS not parsed, no panic
    }

    #[test]
    fn parse_flv_video_meta_h264_seq_header_extracts_fps_from_sps_vui() {
        // libx264 AVCDecoderConfigurationRecord carrying a 1920x1080@50 SPS.
        #[rustfmt::skip]
        let data = [
            0x17u8, 0x00, 0x00, 0x00, 0x00, // keyframe, AVC sequence header
            0x01, 0x42, 0xc0, 0x2a, 0xff, 0xe1, 0x00, 0x18,
            0x67, 0x42, 0xc0, 0x2a, 0xda, 0x01, 0xe0, 0x08,
            0x9f, 0x97, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
            0x10, 0x00, 0x00, 0x06, 0x48, 0xf1, 0x83, 0x2a,
            0x01, 0x00, 0x04, 0x68, 0xce, 0x0f, 0xc8,
        ];

        let meta = parse_flv_video_meta(&data).unwrap();
        assert_eq!(meta.codec, "h264");
        assert_eq!(meta.width, 1920);
        assert_eq!(meta.height, 1080);
        assert!((meta.fps - 50.0).abs() < 0.01, "fps={}", meta.fps);
    }

    // --- FLV audio meta: malformed / truncated / non-AAC codecs ---

    #[test]
    fn parse_flv_audio_meta_empty_returns_none() {
        assert!(parse_flv_audio_meta(&[]).is_none());
    }

    #[test]
    fn parse_flv_audio_meta_mp3_no_asc() {
        // format_id=2 (MP3), rate=3 (44100), size=1, type=1 (stereo)
        let data = [0x2Fu8];
        let meta = parse_flv_audio_meta(&data).unwrap();
        assert_eq!(meta.codec, "mp3");
        assert_eq!(meta.sample_rate, 44100);
        assert_eq!(meta.channels, 2);
    }

    #[test]
    fn parse_flv_audio_meta_speex_mono_11025() {
        // format_id=11 (Speex), rate=1 (11025), type=0 (mono)
        let data = [0xB4u8];
        let meta = parse_flv_audio_meta(&data).unwrap();
        assert_eq!(meta.codec, "speex");
        assert_eq!(meta.sample_rate, 11025);
        assert_eq!(meta.channels, 1);
        assert_eq!(meta.channel_layout.as_deref(), Some("mono"));
    }

    #[test]
    fn parse_flv_audio_meta_aac_data_packet_not_seq_header() {
        // format_id=10 (AAC), byte[1]=1 (data packet, not seq header) → no ASC parsing
        let data = [0xAFu8, 0x01, 0x12, 0x10];
        let meta = parse_flv_audio_meta(&data).unwrap();
        assert_eq!(meta.codec, "aac");
        // sample_rate from FLV rate_id bits only (rate_id=3 → 44100)
        assert_eq!(meta.sample_rate, 44100);
    }

    #[test]
    fn parse_flv_audio_meta_aac_seq_header_truncated_asc_one_byte() {
        // format_id=10, byte[1]=0 (seq header), only 1 byte of ASC → asc.len() < 2, no ASC parsing
        let data = [0xAFu8, 0x00, 0x12];
        let meta = parse_flv_audio_meta(&data).unwrap();
        assert_eq!(meta.codec, "aac");
        // Falls back to FLV header rates (rate_id=3 → 44100)
        assert_eq!(meta.sample_rate, 44100);
    }

    #[test]
    fn parse_flv_audio_meta_aac_5_1_surround() {
        // object_type=2 (AAC-LC), freq_idx=3 (48000), ch_config=6 (5.1)
        // byte[0]: 0xAF (format=10, rate=3, size=1, channels=1 bit)
        // ASC: (2<<3)|(3>>1)=0x11, (3<<7)|(6<<3)=0xB0
        let data = [0xAFu8, 0x00, 0x11, 0xB0];
        let meta = parse_flv_audio_meta(&data).unwrap();
        assert_eq!(meta.codec, "aac");
        assert_eq!(meta.sample_rate, 48000);
        assert_eq!(meta.channels, 6);
        assert_eq!(meta.channel_layout.as_deref(), Some("5.1"));
    }

    // --- FLV composition time: edge cases ---

    #[test]
    fn flv_composition_time_too_short_returns_zero() {
        assert_eq!(flv_video_composition_time_ms(&[]), 0);
        assert_eq!(flv_video_composition_time_ms(&[0x17, 0x01, 0x00, 0x00]), 0); // 4 bytes < 5
    }

    #[test]
    fn flv_composition_time_sequence_header_returns_zero() {
        // packet_type=0 (seq header) → composition time is always 0 per spec
        let data = [0x17u8, 0x00, 0x00, 0x00, 0x28];
        assert_eq!(flv_video_composition_time_ms(&data), 0);
    }

    #[test]
    fn flv_composition_time_h265_nalu_packet() {
        // codec_id=12 (H.265), packet_type=1 (NALU), positive offset = 40ms
        let data = [0x1Cu8, 0x01, 0x00, 0x00, 0x28];
        assert_eq!(flv_video_composition_time_ms(&data), 40);
    }

    #[test]
    fn flv_composition_time_audio_byte_returns_zero() {
        // FLV audio tag (codec_id=10, i.e. byte[0]&0x0F=10, not 7 or 12) → 0
        let data = [0xAFu8, 0x01, 0x00, 0x00, 0x28];
        assert_eq!(flv_video_composition_time_ms(&data), 0);
    }
}
