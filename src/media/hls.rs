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

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use tokio_util::sync::CancellationToken;

use crate::media::codec::{audio_for_ts_into, video_for_ts_into};
use crate::media::engine::MediaEngine;
use crate::media::mpegts::TsMuxer;
use crate::media::ring_buffer::{DtsEnforcer, MediaType, Reader, RingBuffer};

const TARGET_DURATION_SECS: f64 = 6.0;
const MIN_SEGMENT_SECS: f64 = 1.0;
const SEGMENT_CAPACITY: usize = 8 * 1024 * 1024;
// Keep a longer live window so preview clients can still fetch segments that are
// still referenced by the playlist while the stream is moving forward.
const MAX_SEGMENTS: usize = 20;

struct HlsSegment {
    index: u64,
    duration: f64,
    data: Bytes,
}

pub struct HlsStore {
    inner: Mutex<HlsStoreInner>,
}

struct HlsStoreInner {
    segments: VecDeque<HlsSegment>,
    next_index: u64,
    target_duration: f64,
}

impl Default for HlsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HlsStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HlsStoreInner {
                segments: VecDeque::new(),
                next_index: 0,
                target_duration: TARGET_DURATION_SECS,
            }),
        }
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
        while inner.segments.len() > MAX_SEGMENTS {
            inner.segments.pop_front();
        }
    }

    pub fn get_playlist(&self) -> Option<String> {
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
            m3u8.push_str(&format!(
                "#EXTINF:{:.3},\nseg{}.ts\n",
                seg.duration, seg.index
            ));
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
}

pub async fn start_hls_segmenter(
    pipeline_id: String,
    store: Arc<HlsStore>,
    ring_buffer: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel_token: CancellationToken,
) {
    let metrics = engine
        .get_or_create_stage_metrics(&pipeline_id, "hls")
        .await;
    let mut reader = Reader::new(format!("hls:{}", pipeline_id), ring_buffer.clone());
    let mut packets = Vec::with_capacity(32);
    let mut muxer: Option<(
        TsMuxer,
        std::sync::Arc<Vec<crate::media::engine::AudioMeta>>,
    )> = None;
    let mut dts_enforcer: Option<DtsEnforcer> = None;
    let mut has_video = false;
    let mut nalu_len_size: usize = 4;
    // Pre-populate SPS/PPS cache from the engine's stored FLV sequence header.
    // This handles the case where the HLS task starts after the seq header has
    // already passed through the ring buffer (e.g. late-joining consumers).
    let mut sps_pps_cache: Vec<u8> = {
        let (vsh, _) = engine.get_sequence_headers(&pipeline_id).await;
        if let Some(ref flv_sh) = vsh {
            if flv_sh.len() > 5 {
                let (nls, annexb) = crate::media::codec::parse_avcc_config(&flv_sh[5..]);
                nalu_len_size = nls;
                annexb
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    };
    let mut accumulator = BytesMut::with_capacity(SEGMENT_CAPACITY);
    let mut segment_start = Instant::now();
    let mut got_first_keyframe = false;
    let mut video_conv_buf = Vec::<u8>::new();
    let mut audio_conv_buf = Vec::<u8>::new();

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
                                if elapsed >= MIN_SEGMENT_SECS && !accumulator.is_empty() {
                                    store.push_segment(elapsed, accumulator.split().freeze());
                                    accumulator.reserve(SEGMENT_CAPACITY);
                                    segment_start = Instant::now();
                                }
                            }
                            got_first_keyframe = true;
                        }

                        if !got_first_keyframe {
                            continue;
                        }

                        metrics.record_in(packet.payload.len() as u64);

                        // Lazily create the muxer and DtsEnforcer once we have ingest metadata.
                        // Wait for video metadata to avoid creating a muxer with zero audio
                        // streams when the probe hasn't completed yet.
                        let (mux, tracks) = match &mut muxer {
                            Some((m, t)) => (m, t),
                            None => {
                                let (video, audio_tracks) = loop {
                                    if cancel_token.is_cancelled() {
                                        return;
                                    }
                                    if let Some(tracks) = ring_buffer.audio_tracks() {
                                        let ingests = engine.active_ingests.read().await;
                                        if let Some(i) = ingests.get(&pipeline_id)
                                            && let Some(video) = i.video.clone()
                                        {
                                            break (Some(video), std::sync::Arc::new(tracks.to_vec()));
                                        }
                                    }
                                    let result = {
                                        let ingests = engine.active_ingests.read().await;
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
                                has_video = video.is_some();
                                let m = TsMuxer::new(video.as_ref(), &audio_tracks);
                                let num_streams = video.is_some() as usize + audio_tracks.len();
                                dts_enforcer = Some(DtsEnforcer::new(num_streams));
                                muxer = Some((m, audio_tracks));
                                let (m, t) = muxer.as_mut().expect("muxer just initialized");
                                (m, t)
                            }
                        };

                        let raw_payload: &[u8] = match packet.media_type {
                            MediaType::Video => {
                                match video_for_ts_into(&packet.payload, packet.format, &mut nalu_len_size, &mut sps_pps_cache, &mut video_conv_buf) {
                                    Some(p) => p,
                                    None => continue,
                                }
                            }
                            MediaType::Audio => {
                                let track = tracks.iter()
                                    .find(|a| a.track_index == packet.track_index)
                                    .or(tracks.first());
                                let (sr, ch) = track.map(|a| (a.sample_rate, a.channels)).unwrap_or((48000, 1));
                                match audio_for_ts_into(&packet.payload, packet.format, sr, ch, &mut audio_conv_buf) {
                                    Some(p) => p,
                                    None => continue,
                                }
                            }
                        };

                        let t0 = Instant::now();
                        let stream_idx = match packet.media_type {
                            MediaType::Video => 0,
                            MediaType::Audio => match tracks
                                .iter()
                                .position(|a| a.track_index == packet.track_index)
                            {
                                Some(i) => i + (has_video as usize),
                                None => continue, // unknown track — skip to avoid DTS corruption
                            },
                        };
                        let (pts, dts) = dts_enforcer
                            .as_mut()
                            .map(|de| de.enforce(stream_idx, packet.pts, packet.dts))
                            .unwrap_or((packet.pts, packet.dts));
                        let ts_bytes = mux.mux_packet(
                            packet.media_type,
                            packet.track_index,
                            pts,
                            dts,
                            packet.is_keyframe,
                            raw_payload,
                        );
                        metrics.record_processing(t0.elapsed().as_micros() as u64);
                        metrics.record_out(ts_bytes.len() as u64);
                        accumulator.extend_from_slice(ts_bytes);
                    }
                }
            }
        }
    }

    engine.remove_stage_metrics(&pipeline_id, "hls").await;

    // Flush remaining data as final segment
    if !accumulator.is_empty() {
        let elapsed = segment_start.elapsed().as_secs_f64();
        store.push_segment(elapsed, accumulator.freeze());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_references_stored_segments() {
        let store = HlsStore::new();
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
    fn target_duration_tracks_longest_segment() {
        let store = HlsStore::new();
        store.push_segment(7.2, Bytes::from_static(b"long-segment"));

        let playlist = store.get_playlist().expect("playlist");
        assert!(playlist.contains("#EXT-X-TARGETDURATION:8"));
    }

    #[test]
    fn playlist_window_is_bounded_and_advances_media_sequence() {
        let store = HlsStore::new();
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
    fn keeps_a_longer_live_window_for_preview_clients() {
        let store = HlsStore::new();
        for index in 0..14u64 {
            store.push_segment(2.0, Bytes::from(format!("segment-{index}").into_bytes()));
        }

        assert!(store.get_segment(3).is_some());
        assert!(store.get_segment(13).is_some());
    }

    #[test]
    fn clear_resets_segments_and_index() {
        let store = HlsStore::new();
        store.push_segment(2.0, Bytes::from_static(b"data"));
        store.clear();
        assert!(store.get_playlist().is_none());
        assert!(store.get_segment(0).is_none());
    }

    #[test]
    fn empty_store_playlist_returns_none() {
        assert!(HlsStore::new().get_playlist().is_none());
    }

    #[test]
    fn get_segment_nonexistent_returns_none() {
        let store = HlsStore::new();
        store.push_segment(2.0, Bytes::from_static(b"data"));
        assert!(store.get_segment(999).is_none());
    }

    #[test]
    fn get_segment_finds_by_exact_index() {
        let store = HlsStore::new();
        store.push_segment(2.0, Bytes::from_static(b"first"));
        store.push_segment(3.0, Bytes::from_static(b"second"));
        assert_eq!(store.get_segment(1).as_deref(), Some(b"second".as_slice()));
    }

    #[test]
    fn target_duration_never_decreases() {
        let store = HlsStore::new();
        store.push_segment(8.0, Bytes::from_static(b"long"));
        store.push_segment(2.0, Bytes::from_static(b"short"));
        let playlist = store.get_playlist().unwrap();
        assert!(playlist.contains("#EXT-X-TARGETDURATION:8"));
        assert!(!playlist.contains("#EXT-X-TARGETDURATION:2"));
    }

    #[test]
    fn exact_max_segments_does_not_evict_first() {
        let store = HlsStore::new();
        for i in 0..MAX_SEGMENTS as u64 {
            store.push_segment(2.0, Bytes::from(i.to_be_bytes().to_vec()));
        }
        assert!(store.get_segment(0).is_some());
    }

    #[test]
    fn new_store_has_empty_initial_state() {
        let store = HlsStore::new();
        assert!(store.get_playlist().is_none());
        assert!(store.get_segment(0).is_none());
    }

    #[test]
    fn push_segment_assigns_sequential_indices() {
        let store = HlsStore::new();
        store.push_segment(1.0, Bytes::from_static(b"a"));
        store.push_segment(1.0, Bytes::from_static(b"b"));
        store.push_segment(1.0, Bytes::from_static(b"c"));
        assert!(store.get_segment(0).is_some());
        assert!(store.get_segment(1).is_some());
        assert!(store.get_segment(2).is_some());
    }
}
