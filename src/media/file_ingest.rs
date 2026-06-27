use crate::media::avio::{CustomOutput, MemoryQueue};
use crate::media::engine::{MediaEngine, StageMetrics};
use crate::media::mpegts::TsDemuxer;
use crate::media::ring_buffer::{MediaPacket, MediaType, RingBuffer};
use ffmpeg_next::{codec, encoder, format, media};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

type KeyframeTimes = Arc<std::sync::Mutex<Vec<i64>>>;
type IngestRuntime = (Arc<AtomicU64>, Arc<StageMetrics>, KeyframeTimes);

#[derive(Default)]
struct LoopTimestampState {
    offset_ms: i64,
    pass_max_timestamp_ms: Option<i64>,
    pass_packet_count: usize,
}

impl LoopTimestampState {
    fn begin_pass(&mut self) {
        self.pass_max_timestamp_ms = None;
        self.pass_packet_count = 0;
    }

    fn apply(&mut self, packet: &mut MediaPacket) {
        packet.pts = packet.pts.saturating_add(self.offset_ms);
        packet.dts = packet.dts.saturating_add(self.offset_ms);
        self.pass_packet_count += 1;
        let packet_max = packet.pts.max(packet.dts);
        self.pass_max_timestamp_ms = Some(
            self.pass_max_timestamp_ms
                .map_or(packet_max, |current| current.max(packet_max)),
        );
    }

    fn finish_pass(&mut self) {
        if let Some(max_timestamp_ms) = self.pass_max_timestamp_ms {
            self.offset_ms = max_timestamp_ms.saturating_add(1);
        }
    }

    fn pass_packet_count(&self) -> usize {
        self.pass_packet_count
    }
}

#[derive(Default)]
pub(crate) struct ContinuousTimestampState {
    offset_ms: i64,
    last_timestamp_ms: Option<i64>,
}

impl ContinuousTimestampState {
    pub(crate) fn apply(&mut self, packet: &mut MediaPacket) {
        let raw_timestamp_ms = packet.pts.max(packet.dts);
        if let Some(last_timestamp_ms) = self.last_timestamp_ms {
            let adjusted_timestamp_ms = raw_timestamp_ms.saturating_add(self.offset_ms);
            if adjusted_timestamp_ms <= last_timestamp_ms {
                self.offset_ms = last_timestamp_ms
                    .saturating_add(1)
                    .saturating_sub(raw_timestamp_ms);
            }
        }

        packet.pts = packet.pts.saturating_add(self.offset_ms);
        packet.dts = packet.dts.saturating_add(self.offset_ms);
        let adjusted_timestamp_ms = packet.pts.max(packet.dts);
        self.last_timestamp_ms = Some(
            self.last_timestamp_ms
                .map_or(adjusted_timestamp_ms, |current| {
                    current.max(adjusted_timestamp_ms)
                }),
        );
    }
}

pub fn use_internal_file_ingest() -> bool {
    crate::env_flag_enabled("RESTREAM_USE_INTERNAL_FILE_INGEST")
}

pub fn parse_start_time_ms(input: &str) -> Result<Option<i64>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if let Ok(seconds) = trimmed.parse::<f64>() {
        if seconds < 0.0 {
            return Err("start_time must be non-negative".to_string());
        }
        return Ok(Some((seconds * 1000.0).round() as i64));
    }

    let parts: Vec<&str> = trimmed.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return Err("start_time must be seconds or MM:SS(.mmm) or HH:MM:SS(.mmm)".to_string());
    }

    let seconds = parts[parts.len() - 1]
        .parse::<f64>()
        .map_err(|_| "invalid seconds component in start_time".to_string())?;
    if seconds < 0.0 {
        return Err("start_time must be non-negative".to_string());
    }

    let minutes = parts[parts.len() - 2]
        .parse::<i64>()
        .map_err(|_| "invalid minutes component in start_time".to_string())?;
    if minutes < 0 {
        return Err("start_time must be non-negative".to_string());
    }

    let hours = if parts.len() == 3 {
        let value = parts[0]
            .parse::<i64>()
            .map_err(|_| "invalid hours component in start_time".to_string())?;
        if value < 0 {
            return Err("start_time must be non-negative".to_string());
        }
        value
    } else {
        0
    };

    let total_ms = (((hours * 3600 + minutes * 60) as f64 + seconds) * 1000.0).round() as i64;
    Ok(Some(total_ms))
}

pub fn spawn_internal_file_ingest(
    engine: Arc<MediaEngine>,
    runtime_handle: Handle,
    ingest_id: String,
    pipeline_id: String,
    file_path: PathBuf,
    start_time: String,
    loop_enabled: bool,
    ring_buffer: Arc<RingBuffer>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let seek_ms = parse_start_time_ms(&start_time)?;
    let engine_for_thread = engine.clone();
    let runtime_for_thread = runtime_handle.clone();
    let ingest_id_for_thread = ingest_id.clone();
    let pipeline_id_for_thread = pipeline_id.clone();
    let handle = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_internal_file_ingest_loop(
                engine_for_thread.clone(),
                runtime_for_thread.clone(),
                &pipeline_id_for_thread,
                &file_path,
                seek_ms,
                loop_enabled,
                ring_buffer,
                cancel.clone(),
            )
        }));

        match result {
            Ok(Err(err)) if !cancel.is_cancelled() => {
                eprintln!(
                    "[file-ingest] internal ingest failed ({}): {}",
                    ingest_id_for_thread, err
                );
            }
            Err(_) if !cancel.is_cancelled() => {
                eprintln!(
                    "[file-ingest] internal ingest panicked ({})",
                    ingest_id_for_thread
                );
            }
            _ => {}
        }

        runtime_for_thread.block_on(async {
            engine_for_thread
                .clear_file_ingest_running(&ingest_id_for_thread)
                .await;
            engine_for_thread
                .unregister_ingest(&pipeline_id_for_thread)
                .await;
        });
    });
    engine.register_os_thread(handle);
    Ok(())
}

fn run_internal_file_ingest_loop(
    engine: Arc<MediaEngine>,
    runtime_handle: Handle,
    pipeline_id: &str,
    file_path: &Path,
    seek_ms: Option<i64>,
    loop_enabled: bool,
    ring_buffer: Arc<RingBuffer>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let (bytes_received, ingest_metrics, cached_keyframe_times) =
        load_ingest_runtime(&engine, &runtime_handle, pipeline_id)?;
    let mut timestamps = LoopTimestampState::default();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        timestamps.begin_pass();
        run_internal_file_ingest_once(
            &engine,
            &runtime_handle,
            pipeline_id,
            file_path,
            seek_ms,
            &ring_buffer,
            &cancel,
            &bytes_received,
            &ingest_metrics,
            &cached_keyframe_times,
            &mut timestamps,
        )?;
        timestamps.finish_pass();

        if cancel.is_cancelled() || !loop_enabled {
            break;
        }

        if timestamps.pass_packet_count() == 0 {
            return Err(
                "Looped file ingest produced no packets; stopping to avoid a tight loop"
                    .to_string(),
            );
        }
    }

    Ok(())
}

fn load_ingest_runtime(
    engine: &Arc<MediaEngine>,
    runtime_handle: &Handle,
    pipeline_id: &str,
) -> Result<IngestRuntime, String> {
    runtime_handle.block_on(async {
        let ingests = engine.active_ingests.read().await;
        let ingest = ingests
            .get(pipeline_id)
            .ok_or_else(|| format!("Active ingest missing for pipeline {pipeline_id}"))?;
        Ok((
            ingest.bytes_received.clone(),
            ingest.metrics.clone(),
            ingest.keyframe_times.clone(),
        ))
    })
}

#[allow(clippy::too_many_arguments)]
fn run_internal_file_ingest_once(
    engine: &Arc<MediaEngine>,
    runtime_handle: &Handle,
    pipeline_id: &str,
    file_path: &Path,
    seek_ms: Option<i64>,
    ring_buffer: &Arc<RingBuffer>,
    cancel: &CancellationToken,
    bytes_received: &Arc<AtomicU64>,
    ingest_metrics: &Arc<StageMetrics>,
    cached_keyframe_times: &KeyframeTimes,
    timestamps: &mut LoopTimestampState,
) -> Result<(), String> {
    let mut ictx = format::input_with_interrupt(&file_path, || cancel.is_cancelled())
        .map_err(|e| format!("Failed to open input file: {e}"))?;

    if let Some(seek_ms) = seek_ms {
        ictx.seek(seek_ms.saturating_mul(1000), ..)
            .map_err(|e| format!("Failed to seek input file: {e}"))?;
    }

    let queue = MemoryQueue::new();
    let mut custom_output =
        CustomOutput::new(&queue, "mpegts").map_err(|e| format!("TS mux setup failed: {e}"))?;
    let octx = custom_output
        .output
        .as_mut()
        .ok_or_else(|| "Failed to acquire TS output context".to_string())?;

    let mut stream_mapping = vec![-1i32; ictx.nb_streams() as usize];
    let mut ist_time_bases = vec![ffmpeg_next::Rational(0, 1); ictx.nb_streams() as usize];
    let mut ost_index = 0i32;

    for (ist_index, ist) in ictx.streams().enumerate() {
        let medium = ist.parameters().medium();
        if medium != media::Type::Audio && medium != media::Type::Video {
            continue;
        }

        stream_mapping[ist_index] = ost_index;
        ist_time_bases[ist_index] = ist.time_base();
        ost_index += 1;

        let mut ost = octx
            .add_stream(encoder::find(codec::Id::None))
            .map_err(|e| format!("Failed to add TS stream: {e}"))?;
        ost.set_parameters(ist.parameters());
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
    }

    octx.set_metadata(ictx.metadata().to_owned());
    octx.write_header()
        .map_err(|e| format!("Failed to write TS header: {e}"))?;

    let mut demuxer = TsDemuxer::new();
    let mut packets = Vec::with_capacity(16);
    let mut probe_sent = false;
    drain_remuxed_ts(
        engine,
        runtime_handle,
        pipeline_id,
        &queue,
        &mut demuxer,
        &mut packets,
        ring_buffer,
        bytes_received,
        ingest_metrics,
        cached_keyframe_times,
        timestamps,
        &mut probe_sent,
    );

    let mut pace_anchor = None;
    for (stream, mut packet) in ictx.packets() {
        if cancel.is_cancelled() {
            break;
        }

        let ist_index = stream.index();
        let mapped_index = stream_mapping.get(ist_index).copied().unwrap_or(-1);
        if mapped_index < 0 {
            continue;
        }

        if let Some(packet_ts_ms) = packet_timestamp_ms(&stream, &packet) {
            pace_packet(cancel, &mut pace_anchor, packet_ts_ms);
            if cancel.is_cancelled() {
                break;
            }
        }

        let ost = octx
            .stream(mapped_index as usize)
            .ok_or_else(|| format!("Missing TS output stream {}", mapped_index))?;
        packet.rescale_ts(ist_time_bases[ist_index], ost.time_base());
        packet.set_position(-1);
        packet.set_stream(mapped_index as usize);
        packet
            .write_interleaved(octx)
            .map_err(|e| format!("Failed to mux TS packet: {e}"))?;

        drain_remuxed_ts(
            engine,
            runtime_handle,
            pipeline_id,
            &queue,
            &mut demuxer,
            &mut packets,
            ring_buffer,
            bytes_received,
            ingest_metrics,
            cached_keyframe_times,
            timestamps,
            &mut probe_sent,
        );
    }

    octx.write_trailer()
        .map_err(|e| format!("Failed to finalize TS mux: {e}"))?;
    drain_remuxed_ts(
        engine,
        runtime_handle,
        pipeline_id,
        &queue,
        &mut demuxer,
        &mut packets,
        ring_buffer,
        bytes_received,
        ingest_metrics,
        cached_keyframe_times,
        timestamps,
        &mut probe_sent,
    );

    demuxer.flush();
    push_demuxed_packets(
        &mut demuxer,
        &mut packets,
        ring_buffer,
        cached_keyframe_times,
        timestamps,
    );
    maybe_publish_probe(
        engine,
        runtime_handle,
        pipeline_id,
        &mut demuxer,
        &mut probe_sent,
    );

    Ok(())
}

fn packet_timestamp_ms(
    stream: &ffmpeg_next::Stream<'_>,
    packet: &ffmpeg_next::Packet,
) -> Option<i64> {
    let ts = packet.dts().or_else(|| packet.pts())?;
    let tb = stream.time_base();
    if tb.1 == 0 {
        return Some(ts);
    }
    Some((ts as i128 * tb.0 as i128 * 1000 / tb.1 as i128) as i64)
}

fn pace_packet(cancel: &CancellationToken, anchor: &mut Option<(i64, Instant)>, packet_ts_ms: i64) {
    if packet_ts_ms < 0 {
        return;
    }

    if anchor.is_none() {
        *anchor = Some((packet_ts_ms, Instant::now()));
        return;
    }

    let (base_ts_ms, start_instant) = anchor.expect("anchor initialized above");
    let desired_ms = packet_ts_ms.saturating_sub(base_ts_ms) as u64;
    let desired = Duration::from_millis(desired_ms);
    let elapsed = start_instant.elapsed();
    if elapsed >= desired {
        return;
    }

    let mut remaining = desired - elapsed;
    while remaining > Duration::ZERO && !cancel.is_cancelled() {
        let slice = remaining.min(Duration::from_millis(25));
        std::thread::sleep(slice);
        remaining = desired.saturating_sub(start_instant.elapsed());
    }
}

#[allow(clippy::too_many_arguments)]
fn drain_remuxed_ts(
    engine: &Arc<MediaEngine>,
    runtime_handle: &Handle,
    pipeline_id: &str,
    queue: &MemoryQueue,
    demuxer: &mut TsDemuxer,
    packets: &mut Vec<MediaPacket>,
    ring_buffer: &Arc<RingBuffer>,
    bytes_received: &Arc<AtomicU64>,
    ingest_metrics: &Arc<StageMetrics>,
    cached_keyframe_times: &KeyframeTimes,
    timestamps: &mut LoopTimestampState,
    probe_sent: &mut bool,
) {
    let mut buf = [0u8; 64 * 1024];

    loop {
        let read = queue.read_nonblocking(&mut buf);
        if read == 0 {
            break;
        }

        demuxer.feed(&buf[..read]);
        push_demuxed_packets(
            demuxer,
            packets,
            ring_buffer,
            cached_keyframe_times,
            timestamps,
        );
        maybe_publish_probe(engine, runtime_handle, pipeline_id, demuxer, probe_sent);
        bytes_received.fetch_add(read as u64, Ordering::Relaxed);
        ingest_metrics.record_in(read as u64);
    }
}

fn push_demuxed_packets(
    demuxer: &mut TsDemuxer,
    packets: &mut Vec<MediaPacket>,
    ring_buffer: &Arc<RingBuffer>,
    cached_keyframe_times: &KeyframeTimes,
    timestamps: &mut LoopTimestampState,
) {
    if demuxer.drain_into(packets) == 0 {
        return;
    }

    for pkt in packets.iter_mut() {
        timestamps.apply(pkt);
    }

    for pkt in packets.iter() {
        if pkt.media_type == MediaType::Video && pkt.is_keyframe {
            let mut times = cached_keyframe_times
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            times.push(pkt.pts);
            if times.len() > 30 {
                times.remove(0);
            }
        }
    }

    ring_buffer.push_batch(packets.drain(..));
}

fn maybe_publish_probe(
    engine: &Arc<MediaEngine>,
    runtime_handle: &Handle,
    pipeline_id: &str,
    demuxer: &mut TsDemuxer,
    probe_sent: &mut bool,
) {
    if *probe_sent {
        return;
    }

    let Some(probe) = demuxer.take_probe() else {
        return;
    };
    *probe_sent = true;
    let first_audio = probe.audio_tracks.first().cloned();
    runtime_handle.block_on(async {
        engine
            .update_ingest_meta(pipeline_id, probe.video, first_audio, None)
            .await;
        if !probe.audio_tracks.is_empty() {
            engine
                .update_ingest_audio_tracks(pipeline_id, probe.audio_tracks)
                .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::parse_start_time_ms;
    use super::spawn_internal_file_ingest;
    use super::{ContinuousTimestampState, LoopTimestampState};
    use crate::media::engine::MediaEngine;
    use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat};
    use bytes::Bytes;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::time::{Duration, sleep};

    #[test]
    fn empty_start_time_is_none() {
        assert_eq!(parse_start_time_ms("").unwrap(), None);
        assert_eq!(parse_start_time_ms("   ").unwrap(), None);
    }

    #[test]
    fn parses_seconds_start_time() {
        assert_eq!(parse_start_time_ms("5").unwrap(), Some(5_000));
        assert_eq!(parse_start_time_ms("1.25").unwrap(), Some(1_250));
    }

    #[test]
    fn parses_colon_delimited_start_time() {
        assert_eq!(parse_start_time_ms("00:00:05").unwrap(), Some(5_000));
        assert_eq!(parse_start_time_ms("01:02:03.5").unwrap(), Some(3_723_500));
        assert_eq!(parse_start_time_ms("02:03.25").unwrap(), Some(123_250));
    }

    #[test]
    fn rejects_invalid_start_time() {
        assert!(parse_start_time_ms("-1").is_err());
        assert!(parse_start_time_ms("1:two").is_err());
        assert!(parse_start_time_ms("1:2:3:4").is_err());
    }

    #[test]
    fn loop_timestamp_state_keeps_replayed_packets_monotonic() {
        let mut timestamps = LoopTimestampState::default();

        timestamps.begin_pass();
        let mut first = test_packet(0, 0);
        let mut second = test_packet(33, 33);
        timestamps.apply(&mut first);
        timestamps.apply(&mut second);
        timestamps.finish_pass();

        assert_eq!(first.pts, 0);
        assert_eq!(second.pts, 33);
        assert_eq!(timestamps.pass_packet_count(), 2);

        timestamps.begin_pass();
        let mut looped_first = test_packet(0, 0);
        let mut looped_second = test_packet(33, 33);
        timestamps.apply(&mut looped_first);
        timestamps.apply(&mut looped_second);
        timestamps.finish_pass();

        assert_eq!(looped_first.pts, 34);
        assert_eq!(looped_first.dts, 34);
        assert_eq!(looped_second.pts, 67);
        assert_eq!(looped_second.dts, 67);
        assert_eq!(timestamps.pass_packet_count(), 2);
    }

    #[test]
    fn loop_timestamp_state_reports_empty_passes() {
        let mut timestamps = LoopTimestampState::default();
        timestamps.begin_pass();
        timestamps.finish_pass();

        assert_eq!(timestamps.pass_packet_count(), 0);
    }

    #[test]
    fn continuous_timestamp_state_offsets_replayed_subprocess_packets() {
        let mut timestamps = ContinuousTimestampState::default();

        let mut first = test_packet(0, 0);
        let mut second = test_packet(40, 40);
        timestamps.apply(&mut first);
        timestamps.apply(&mut second);

        let mut replayed_first = test_packet(0, 0);
        let mut replayed_second = test_packet(40, 40);
        timestamps.apply(&mut replayed_first);
        timestamps.apply(&mut replayed_second);

        assert_eq!(first.pts, 0);
        assert_eq!(second.pts, 40);
        assert_eq!(replayed_first.pts, 41);
        assert_eq!(replayed_first.dts, 41);
        assert_eq!(replayed_second.pts, 81);
        assert_eq!(replayed_second.dts, 81);
    }

    fn test_packet(pts: i64, dts: i64) -> MediaPacket {
        MediaPacket {
            media_type: MediaType::Video,
            format: PayloadFormat::Raw,
            is_keyframe: false,
            track_index: 0,
            pts,
            dts,
            payload: Bytes::from_static(b"packet"),
        }
    }

    #[tokio::test]
    async fn internal_file_ingest_pushes_packets_and_stays_registered() {
        let engine = Arc::new(MediaEngine::new());
        let pipeline_id = "pipe-file-ingest-test";
        let ingest_id = "ing-file-ingest-test";
        let stream_key = "file-ingest-test-key";
        let ring_buffer = engine.get_or_create_pipeline(pipeline_id).await;
        let cancel = engine
            .try_register_ingest(pipeline_id, stream_key, "file")
            .await
            .expect("register ingest");

        engine.mark_file_ingest_running(ingest_id).await;
        spawn_internal_file_ingest(
            engine.clone(),
            tokio::runtime::Handle::current(),
            ingest_id.to_string(),
            pipeline_id.to_string(),
            PathBuf::from("media/sadhguru-live.mp4"),
            String::new(),
            false,
            ring_buffer.clone(),
            cancel.clone(),
        )
        .expect("spawn internal ingest");

        sleep(Duration::from_secs(2)).await;

        assert!(
            engine.active_ingests.read().await.contains_key(pipeline_id),
            "internal ingest should still be registered while streaming"
        );
        assert!(
            ring_buffer.get_write_idx() > 0,
            "internal ingest should have produced media packets after startup"
        );

        cancel.cancel();
        sleep(Duration::from_millis(250)).await;
    }
}
