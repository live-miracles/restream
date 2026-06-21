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

use crate::media::engine::MediaEngine;
use crate::media::mpegts::TsMuxer;
use crate::media::ring_buffer::{MediaType, Reader, RingBuffer};

const TARGET_DURATION_SECS: f64 = 6.0;
const MIN_SEGMENT_SECS: f64 = 1.0;
const SEGMENT_CAPACITY: usize = 8 * 1024 * 1024;
// 10 segments × ~6s target = ~60s sliding window
const MAX_SEGMENTS: usize = 10;

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

    pub fn push_segment(&self, duration: f64, data: Bytes) {
        let mut inner = self.inner.lock().unwrap();
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
        let inner = self.inner.lock().unwrap();
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
        let inner = self.inner.lock().unwrap();
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
    let mut reader = Reader::new(ring_buffer);
    let mut packets = Vec::with_capacity(32);
    let mut muxer: Option<TsMuxer> = None;
    let mut accumulator = BytesMut::with_capacity(SEGMENT_CAPACITY);
    let mut segment_start = Instant::now();
    let mut got_first_keyframe = false;

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

                        // Lazily create the muxer once we have ingest metadata
                        let mux = match &mut muxer {
                            Some(m) => m,
                            None => {
                                let ingests = engine.active_ingests.read().await;
                                let ingest = match ingests.get(&pipeline_id) {
                                    Some(i) => i,
                                    None => continue,
                                };
                                let video = ingest.video.as_ref();
                                let audio_tracks = ingest.audio_tracks.lock().unwrap();
                                muxer = Some(TsMuxer::new(video, &audio_tracks));
                                drop(audio_tracks);
                                drop(ingests);
                                muxer.as_mut().unwrap()
                            }
                        };

                        let t0 = Instant::now();
                        let ts_bytes = mux.mux_packet(
                            packet.media_type,
                            packet.track_index,
                            packet.pts,
                            packet.dts,
                            packet.is_keyframe,
                            &packet.payload,
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
}
