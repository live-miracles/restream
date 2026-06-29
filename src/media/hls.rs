//! In-memory HLS segmenter — muxes to MPEG-TS via in-house `TsMuxer`, splits on
//! keyframe boundaries, and stores segments in `HlsStore`. No disk I/O, no FFmpeg,
//! no OS threads on the hot path. Segments are served directly from memory by the
//! Axum API.
//!
//! # Segment Lifecycle
//!
//! ```text
//! RingBuffer → TsMuxer (inline) → segment accumulator → HlsStore
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use tokio_util::sync::CancellationToken;

use crate::media::engine::{AudioMeta, MediaEngine, VideoMeta};
use crate::media::feeder::{PacketFeedConfig, TsPacketFeeder};
use crate::media::ring_buffer::{MediaType, Reader, RingBuffer};

const TARGET_DURATION_SECS: f64 = 6.0;
const MIN_SEGMENT_SECS: f64 = 1.0;
const SEGMENT_CAPACITY: usize = 8 * 1024 * 1024;
// Keep a longer live window so preview clients can still fetch segments that are
// still referenced by the playlist while the stream is moving forward.
const MAX_SEGMENTS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HlsConfig {
    pub min_segment_secs: f64,
    pub segment_capacity: usize,
    pub max_segments: usize,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            min_segment_secs: MIN_SEGMENT_SECS,
            segment_capacity: SEGMENT_CAPACITY,
            max_segments: MAX_SEGMENTS,
        }
    }
}

impl HlsConfig {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            min_segment_secs: env_u64("RESTREAM_HLS_MIN_SEGMENT_MS")
                .map(|ms| ms.max(1) as f64 / 1000.0)
                .unwrap_or(defaults.min_segment_secs),
            segment_capacity: env_usize("RESTREAM_HLS_SEGMENT_CAPACITY_BYTES")
                .map(|bytes| bytes.max(188))
                .unwrap_or(defaults.segment_capacity),
            max_segments: env_usize("RESTREAM_HLS_MAX_SEGMENTS")
                .map(|segments| segments.max(1))
                .unwrap_or(defaults.max_segments),
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.parse().ok()
}

struct HlsSegment {
    index: u64,
    duration: f64,
    data: Bytes,
}

#[derive(Clone)]
pub struct HlsSegmentSnapshot {
    pub index: u64,
    pub data: Bytes,
}

pub struct HlsStoreSnapshot {
    pub playlist: String,
    pub segments: Vec<HlsSegmentSnapshot>,
}

pub struct HlsStore {
    inner: Mutex<HlsStoreInner>,
    config: HlsConfig,
}

struct HlsStoreInner {
    segments: VecDeque<HlsSegment>,
    next_index: u64,
    target_duration: f64,
    video: Option<VideoMeta>,
    audio_tracks: Vec<AudioMeta>,
    variant_segments: HashMap<(u64, HlsSegmentVariant), Bytes>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HlsSegmentVariant {
    Video,
    Audio(u32),
}

impl Default for HlsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HlsStore {
    pub fn new() -> Self {
        Self::with_config(HlsConfig::from_env())
    }

    pub fn with_config(config: HlsConfig) -> Self {
        Self {
            inner: Mutex::new(HlsStoreInner {
                segments: VecDeque::new(),
                next_index: 0,
                target_duration: TARGET_DURATION_SECS,
                video: None,
                audio_tracks: Vec::new(),
                variant_segments: HashMap::new(),
            }),
            config,
        }
    }

    pub fn config(&self) -> HlsConfig {
        self.config
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.segments.clear();
        inner.next_index = 0;
        inner.target_duration = TARGET_DURATION_SECS;
    }

    pub fn push_segment(&self, duration: f64, data: Bytes) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let index = inner.next_index;
        inner.next_index += 1;
        if duration > inner.target_duration {
            inner.target_duration = duration.ceil();
        }
        inner.segments.push_back(HlsSegment {
            index,
            duration,
            data,
        });
        while inner.segments.len() > self.config.max_segments {
            if let Some(segment) = inner.segments.pop_front() {
                inner
                    .variant_segments
                    .retain(|(segment_index, _), _| *segment_index != segment.index);
            }
        }
    }

    pub fn get_playlist(&self) -> Option<String> {
        self.get_playlist_with_segment_uri(|index| format!("seg{index}.ts"))
    }

    pub fn get_playlist_with_segment_uri<F>(&self, mut segment_uri: F) -> Option<String>
    where
        F: FnMut(u64) -> String,
    {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.segments.is_empty() {
            return None;
        }
        let first_seq = inner.segments.front().map(|s| s.index).unwrap_or(0);
        let target_dur = inner.target_duration.ceil() as u64;

        let mut m3u8 = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
            target_dur, first_seq
        );
        for seg in &inner.segments {
            let uri = segment_uri(seg.index);
            m3u8.push_str(&format!("#EXTINF:{:.3},\n{}\n", seg.duration, uri));
        }
        Some(m3u8)
    }

    pub fn get_segment(&self, index: u64) -> Option<Bytes> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .segments
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.data.clone())
    }

    pub fn set_stream_metadata(&self, video: Option<VideoMeta>, audio_tracks: Vec<AudioMeta>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.video = video;
        inner.audio_tracks = audio_tracks;
    }

    pub fn stream_metadata(&self) -> (Option<VideoMeta>, Vec<AudioMeta>) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (inner.video.clone(), inner.audio_tracks.clone())
    }

    pub fn get_variant_segment(&self, index: u64, variant: HlsSegmentVariant) -> Option<Bytes> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.variant_segments.get(&(index, variant)).cloned()
    }

    pub fn put_variant_segment(&self, index: u64, variant: HlsSegmentVariant, data: Bytes) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.segments.iter().any(|segment| segment.index == index) {
            inner.variant_segments.insert((index, variant), data);
        }
    }

    pub fn snapshot(&self) -> Option<HlsStoreSnapshot> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.segments.is_empty() {
            return None;
        }

        let first_seq = inner.segments.front().map(|s| s.index).unwrap_or(0);
        let target_dur = inner.target_duration.ceil() as u64;
        let mut playlist = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
            target_dur, first_seq
        );
        let mut segments = Vec::with_capacity(inner.segments.len());
        for seg in &inner.segments {
            playlist.push_str(&format!(
                "#EXTINF:{:.3},\nseg{}.ts\n",
                seg.duration, seg.index
            ));
            segments.push(HlsSegmentSnapshot {
                index: seg.index,
                data: seg.data.clone(),
            });
        }
        Some(HlsStoreSnapshot { playlist, segments })
    }
}

pub async fn start_hls_segmenter(
    pipeline_id: String,
    store: Arc<HlsStore>,
    ring_buffer: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel_token: CancellationToken,
) {
    let hls_stage_key = crate::domain::stage::StageKey::new(
        pipeline_id.as_str(),
        crate::domain::stage::StageKind::hls(),
    );
    let metrics = engine
        .get_or_create_stage_metrics(hls_stage_key.clone())
        .await;
    engine
        .runtime
        .event_log
        .emit(crate::events::EventKind::StageStarted {
            pipeline_id: pipeline_id.clone(),
            encoding: "hls".to_string(),
        });
    let mut reader = Reader::new(format!("hls:{}", pipeline_id), ring_buffer.clone());
    let mut packets = Vec::with_capacity(32);
    let mut feeder: Option<TsPacketFeeder> = None;
    // Pre-populate SPS/PPS cache from the engine's stored FLV sequence header.
    // This handles the case where the HLS task starts after the seq header has
    // already passed through the ring buffer (e.g. late-joining consumers).
    let (video_sequence_header, _) = engine.get_sequence_headers(&pipeline_id).await;
    let config = store.config();
    let mut accumulator = BytesMut::with_capacity(config.segment_capacity);
    let mut segment_start = Instant::now();
    let mut got_first_keyframe = false;
    let mut ts_packet_buf = Vec::<u8>::with_capacity(65536);

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                loop {
                    packets.clear();
                    match reader.pull_burst(&mut packets, 32) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }

                    for packet in &packets {
                        if packet.media_type == MediaType::Video && packet.is_keyframe {
                            if got_first_keyframe {
                                let elapsed = segment_start.elapsed().as_secs_f64();
                                if elapsed >= config.min_segment_secs && !accumulator.is_empty() {
                                    store.push_segment(elapsed, accumulator.split().freeze());
                                    accumulator.reserve(config.segment_capacity);
                                    segment_start = Instant::now();
                                }
                            }
                            got_first_keyframe = true;
                        }

                        if !got_first_keyframe {
                            continue;
                        }

                        metrics.record_in(packet.payload.len() as u64);

                        // Lazily create the feeder once we have ingest metadata.
                        // Wait for video metadata to avoid creating a muxer with zero audio
                        // streams when the probe hasn't completed yet.
                        if feeder.is_none() {
                            let (video, audio_tracks) = loop {
                                if cancel_token.is_cancelled() {
                                    engine.remove_stage_metrics(&hls_stage_key).await;
                                    engine.runtime.event_log.emit(crate::events::EventKind::StageStopped {
                                        pipeline_id: pipeline_id.clone(),
                                        encoding: "hls".to_string(),
                                    });
                                    return;
                                }
                                if let Some(tracks) = ring_buffer.audio_tracks() {
                                    let ingests = engine.ingests.active.read().await;
                                    if let Some(i) = ingests.get(&pipeline_id)
                                        && let Some(video) = i.video.clone()
                                    {
                                        break (Some(video), std::sync::Arc::new(tracks.to_vec()));
                                    }
                                }
                                let result = {
                                    let ingests = engine.ingests.active.read().await;
                                    ingests.get(&pipeline_id).and_then(|i| {
                                        let video = i.video.clone();
                                        video.as_ref()?;
                                        let lock = i.audio_tracks.lock().unwrap_or_else(|e| e.into_inner());
                                        let tracks = if lock.is_empty()
                                            && let Some(audio) = i.audio.clone() {
                                                std::sync::Arc::new(vec![audio])
                                            } else {
                                                std::sync::Arc::clone(&lock)
                                            };
                                        Some((video, tracks))
                                    })
                                };
                                if let Some(meta) = result {
                                    break meta;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            };
                            let audio_tracks_vec = audio_tracks.as_ref().clone();
                            feeder = Some(TsPacketFeeder::new(
                                video.as_ref(),
                                audio_tracks,
                                PacketFeedConfig {
                                    video_sequence_header: video_sequence_header.as_ref().map(|v| v.to_vec()),
                                    ..PacketFeedConfig::default()
                                },
                            ));
                            store.set_stream_metadata(video.clone(), audio_tracks_vec);
                        }

                        let Some(ref mut feeder) = feeder else {
                            continue;
                        };

                        let t0 = Instant::now();
                        ts_packet_buf.clear();
                        let wrote = feeder.extend_ts_for_packet(packet, &mut ts_packet_buf);
                        metrics.record_processing(t0.elapsed().as_micros() as u64);
                        if wrote {
                            metrics.record_out(ts_packet_buf.len() as u64);
                            accumulator.extend_from_slice(&ts_packet_buf);
                        }
                    }
                }
            }
        }
    }

    engine.remove_stage_metrics(&hls_stage_key).await;
    engine
        .runtime
        .event_log
        .emit(crate::events::EventKind::StageStopped {
            pipeline_id: pipeline_id.clone(),
            encoding: "hls".to_string(),
        });

    // Flush remaining data as final segment
    if !accumulator.is_empty() {
        let elapsed = segment_start.elapsed().as_secs_f64();
        store.push_segment(elapsed, accumulator.freeze());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env_lock<T>(f: impl FnOnce() -> T) -> T {
        let guard: MutexGuard<'_, ()> = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = f();
        drop(guard);
        result
    }

    fn test_store() -> HlsStore {
        HlsStore::with_config(HlsConfig::default())
    }

    #[test]
    fn playlist_references_stored_segments() {
        let store = test_store();
        store.push_segment(2.25, Bytes::from_static(b"segment-zero"));
        store.push_segment(3.5, Bytes::from_static(b"segment-one"));

        let playlist = store.get_playlist().expect("playlist");
        assert!(playlist.contains("#EXT-X-TARGETDURATION:6"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(playlist.contains("#EXTINF:2.250,\nseg0.ts"));
        assert!(playlist.contains("#EXTINF:3.500,\nseg1.ts"));
        assert_eq!(
            store.get_segment(1).as_deref(),
            Some(b"segment-one".as_slice())
        );
    }

    #[test]
    fn playlist_can_reference_variant_segment_paths() {
        let store = test_store();
        store.push_segment(2.25, Bytes::from_static(b"segment-zero"));
        store.push_segment(3.5, Bytes::from_static(b"segment-one"));

        let playlist = store
            .get_playlist_with_segment_uri(|index| format!("audio/15/seg{index}.ts"))
            .expect("playlist");

        assert!(playlist.contains("#EXTINF:2.250,\naudio/15/seg0.ts"));
        assert!(playlist.contains("#EXTINF:3.500,\naudio/15/seg1.ts"));
    }

    #[test]
    fn variant_cache_evicts_with_source_segments() {
        let store = HlsStore::with_config(HlsConfig {
            max_segments: 1,
            ..HlsConfig::default()
        });
        store.push_segment(2.0, Bytes::from_static(b"source-zero"));
        store.put_variant_segment(
            0,
            HlsSegmentVariant::Audio(3),
            Bytes::from_static(b"variant-zero"),
        );
        assert_eq!(
            store
                .get_variant_segment(0, HlsSegmentVariant::Audio(3))
                .as_deref(),
            Some(b"variant-zero".as_slice())
        );

        store.push_segment(2.0, Bytes::from_static(b"source-one"));

        assert!(store.get_segment(0).is_none());
        assert!(
            store
                .get_variant_segment(0, HlsSegmentVariant::Audio(3))
                .is_none()
        );
    }

    #[test]
    fn target_duration_tracks_longest_segment() {
        let store = test_store();
        store.push_segment(7.2, Bytes::from_static(b"long-segment"));

        let playlist = store.get_playlist().expect("playlist");
        assert!(playlist.contains("#EXT-X-TARGETDURATION:8"));
    }

    #[test]
    fn playlist_window_is_bounded_and_advances_media_sequence() {
        let store = test_store();
        for index in 0..(MAX_SEGMENTS as u64 + 2) {
            store.push_segment(2.0, Bytes::from(index.to_be_bytes().to_vec()));
        }

        let playlist = store.get_playlist().expect("playlist");
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:2"));
        assert!(!playlist.contains("seg0.ts"));
        assert!(!playlist.contains("seg1.ts"));
        assert!(playlist.contains("seg11.ts"));
        assert!(store.get_segment(0).is_none());
        assert!(store.get_segment(2).is_some());
    }

    #[test]
    fn custom_max_segments_controls_live_window() {
        let store = HlsStore::with_config(HlsConfig {
            max_segments: 3,
            ..HlsConfig::default()
        });
        for index in 0..5u64 {
            store.push_segment(2.0, Bytes::from(index.to_be_bytes().to_vec()));
        }

        let playlist = store.get_playlist().expect("playlist");
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:2"));
        assert!(store.get_segment(0).is_none());
        assert!(store.get_segment(1).is_none());
        assert!(store.get_segment(2).is_some());
        assert_eq!(playlist.matches(".ts").count(), 3);
    }

    #[test]
    fn keeps_a_longer_live_window_for_preview_clients() {
        let store = test_store();
        for index in 0..14u64 {
            store.push_segment(2.0, Bytes::from(format!("segment-{index}").into_bytes()));
        }

        assert!(store.get_segment(3).is_some());
        assert!(store.get_segment(13).is_some());
    }

    #[test]
    fn clear_resets_segments_and_index() {
        let store = test_store();
        store.push_segment(2.0, Bytes::from_static(b"data"));
        store.clear();
        assert!(store.get_playlist().is_none());
        assert!(store.get_segment(0).is_none());
    }

    #[test]
    fn empty_store_playlist_returns_none() {
        assert!(test_store().get_playlist().is_none());
    }

    #[test]
    fn get_segment_nonexistent_returns_none() {
        let store = test_store();
        store.push_segment(2.0, Bytes::from_static(b"data"));
        assert!(store.get_segment(999).is_none());
    }

    #[test]
    fn get_segment_finds_by_exact_index() {
        let store = test_store();
        store.push_segment(2.0, Bytes::from_static(b"first"));
        store.push_segment(3.0, Bytes::from_static(b"second"));
        assert_eq!(store.get_segment(1).as_deref(), Some(b"second".as_slice()));
    }

    #[test]
    fn target_duration_never_decreases() {
        let store = test_store();
        store.push_segment(8.0, Bytes::from_static(b"long"));
        store.push_segment(2.0, Bytes::from_static(b"short"));
        let playlist = store.get_playlist().unwrap();
        assert!(playlist.contains("#EXT-X-TARGETDURATION:8"));
        assert!(!playlist.contains("#EXT-X-TARGETDURATION:2"));
    }

    #[test]
    fn exact_max_segments_does_not_evict_first() {
        let store = test_store();
        for i in 0..MAX_SEGMENTS as u64 {
            store.push_segment(2.0, Bytes::from(i.to_be_bytes().to_vec()));
        }
        assert!(store.get_segment(0).is_some());
    }

    #[test]
    fn new_store_has_empty_initial_state() {
        let store = test_store();
        assert!(store.get_playlist().is_none());
        assert!(store.get_segment(0).is_none());
    }

    #[test]
    fn push_segment_assigns_sequential_indices() {
        let store = test_store();
        store.push_segment(1.0, Bytes::from_static(b"a"));
        store.push_segment(1.0, Bytes::from_static(b"b"));
        store.push_segment(1.0, Bytes::from_static(b"c"));
        assert!(store.get_segment(0).is_some());
        assert!(store.get_segment(1).is_some());
        assert!(store.get_segment(2).is_some());
    }

    #[test]
    fn playlist_exact_extinf_format() {
        let store = test_store();
        store.push_segment(2.25, Bytes::new());
        let playlist = store.get_playlist().unwrap();
        assert!(playlist.contains("#EXTINF:2.250,"));
        assert!(playlist.contains("seg0.ts"));
    }

    #[test]
    fn get_segment_returns_none_before_first_index() {
        let store = test_store();
        // Start at index 5
        for _ in 0..5 {
            store.push_segment(2.0, Bytes::new());
        }
        // Clear sets next_index=0, so push 2 more starting at index 0
        store.clear();
        store.push_segment(1.0, Bytes::from_static(b"a"));
        store.push_segment(1.0, Bytes::from_static(b"b"));
        // Now get_segment(5) should be None since it was cleared
        assert!(store.get_segment(5).is_none());
        // And get_segment(0) should exist
        assert!(store.get_segment(0).is_some());
    }

    #[test]
    fn media_sequence_advances_after_eviction() {
        let store = test_store();
        // Fill beyond MAX_SEGMENTS to trigger eviction
        for _ in 0..(MAX_SEGMENTS as u64 + 5) {
            store.push_segment(2.0, Bytes::new());
        }
        let playlist = store.get_playlist().unwrap();
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:5"));
        // Oldest segment should be gone
        assert!(store.get_segment(0).is_none());
    }

    #[test]
    fn playlist_range_covers_entire_window() {
        let store = test_store();
        let n = MAX_SEGMENTS as u64;
        for _ in 0..n {
            store.push_segment(2.0, Bytes::new());
        }
        let playlist = store.get_playlist().unwrap();
        assert!(playlist.contains("seg0.ts"));
        assert!(playlist.contains(&format!("seg{}.ts", n - 1)));
    }

    #[test]
    fn snapshot_returns_none_when_empty() {
        let store = test_store();
        assert!(store.snapshot().is_none());
    }

    #[test]
    fn snapshot_contains_playlist_and_all_segments() {
        let store = test_store();
        store.push_segment(2.0, Bytes::from_static(b"data0"));
        store.push_segment(3.0, Bytes::from_static(b"data1"));

        let snap = store.snapshot().expect("snapshot");
        assert!(snap.playlist.contains("seg0.ts"));
        assert!(snap.playlist.contains("seg1.ts"));
        assert_eq!(snap.segments.len(), 2);
        assert_eq!(snap.segments[0].data.as_ref(), b"data0");
        assert_eq!(snap.segments[1].data.as_ref(), b"data1");
    }

    #[test]
    fn stream_metadata_roundtrip() {
        let store = test_store();

        let (v, a) = store.stream_metadata();
        assert!(v.is_none());
        assert!(a.is_empty());

        let video = crate::media::engine::VideoMeta {
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            fps: 30.0,
            bw: None,
            pid: None,
            language: None,
            title: None,
            profile: None,
            level: None,
            pixel_format: None,
        };
        let audio = crate::media::engine::AudioMeta {
            codec: "aac".into(),
            sample_rate: 48000,
            channels: 2,
            track_index: 0,
            ..Default::default()
        };
        store.set_stream_metadata(Some(video.clone()), vec![audio.clone()]);

        let (v2, a2) = store.stream_metadata();
        assert!(v2.is_some());
        assert_eq!(v2.unwrap().codec, "h264");
        assert_eq!(a2.len(), 1);
        assert_eq!(a2[0].codec, "aac");
    }

    #[test]
    fn hls_config_from_env_uses_defaults_when_unset() {
        with_env_lock(|| {
            unsafe {
                std::env::remove_var("RESTREAM_HLS_MIN_SEGMENT_MS");
                std::env::remove_var("RESTREAM_HLS_SEGMENT_CAPACITY_BYTES");
                std::env::remove_var("RESTREAM_HLS_MAX_SEGMENTS");
            }
            let cfg = HlsConfig::from_env();
            let defaults = HlsConfig::default();
            assert_eq!(cfg.min_segment_secs, defaults.min_segment_secs);
            assert_eq!(cfg.segment_capacity, defaults.segment_capacity);
            assert_eq!(cfg.max_segments, defaults.max_segments);
        });
    }

    #[test]
    fn hls_config_from_env_reads_env_vars() {
        with_env_lock(|| {
            unsafe {
                std::env::set_var("RESTREAM_HLS_MIN_SEGMENT_MS", "500");
                std::env::set_var("RESTREAM_HLS_MAX_SEGMENTS", "5");
            }
            let cfg = HlsConfig::from_env();
            unsafe {
                std::env::remove_var("RESTREAM_HLS_MIN_SEGMENT_MS");
                std::env::remove_var("RESTREAM_HLS_MAX_SEGMENTS");
            }
            assert!((cfg.min_segment_secs - 0.5).abs() < 0.001);
            assert_eq!(cfg.max_segments, 5);
        });
    }

    #[test]
    fn hls_config_from_env_reads_env_vars_when_set_to_custom_capacity() {
        with_env_lock(|| {
            unsafe {
                std::env::set_var("RESTREAM_HLS_SEGMENT_CAPACITY_BYTES", "524288");
                std::env::set_var("RESTREAM_HLS_MAX_SEGMENTS", "9");
            }
            let cfg = HlsConfig::from_env();
            unsafe {
                std::env::remove_var("RESTREAM_HLS_SEGMENT_CAPACITY_BYTES");
                std::env::remove_var("RESTREAM_HLS_MAX_SEGMENTS");
            }
            assert_eq!(cfg.segment_capacity, 524288);
            assert_eq!(cfg.max_segments, 9);
        });
    }

    #[test]
    fn put_variant_segment_ignored_for_unknown_source_index() {
        let store = test_store();
        store.push_segment(2.0, Bytes::from_static(b"seg0"));
        // index 99 doesn't exist — the put should be silently dropped
        store.put_variant_segment(99, HlsSegmentVariant::Audio(0), Bytes::from_static(b"v"));
        assert!(
            store
                .get_variant_segment(99, HlsSegmentVariant::Audio(0))
                .is_none()
        );
    }
}
