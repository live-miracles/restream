//! In-memory HLS segmenter — muxes to MPEG-TS via FFmpeg, splits on keyframe
//! boundaries, and stores segments in a lock-free `HlsStore`. No disk I/O on the
//! hot path. Segments are served directly from memory by the Axum API.
//!
//! # Segment Lifecycle
//!
//! ```text
//! RingBuffer → MemoryQueue → FFmpeg MPEG-TS muxer (OS thread)
//!                                     │
//!                                     ▼
//!                              MemoryQueue (output)
//!                                     │
//!                                     ▼
//!                         Splitter thread (cuts on keyframe signal)
//!                                     │
//!                                     ▼
//!                              HlsStore { segments, playlist }
//! ```
//!
//! The splitter watches an `AtomicBool` set by the input feeder when it writes a
//! keyframe packet. On the output side, it accumulates TS bytes and finalizes the
//! current segment when the flag fires (after a minimum duration to avoid micro-segments).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use tokio_util::sync::CancellationToken;

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

    fn push_segment(&self, duration: f64, data: Bytes) {
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
    _pipeline_id: String,
    store: Arc<HlsStore>,
    ring_buffer: Arc<RingBuffer>,
    cancel_token: CancellationToken,
) {
    let input_queue = Arc::new(crate::media::avio::MemoryQueue::new());
    let output_queue = Arc::new(crate::media::avio::MemoryQueue::new());

    let keyframe_signal = Arc::new(AtomicBool::new(false));

    // OS thread: FFmpeg MPEG-TS muxer
    let iq = input_queue.clone();
    let oq = output_queue.clone();
    let ct = cancel_token.clone();
    std::thread::spawn(move || {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_ts_muxer(iq, oq, ct)));
        match result {
            Ok(Err(e)) => eprintln!("[hls] MPEG-TS muxer failed: {:?}", e),
            Err(_) => eprintln!("[hls] MPEG-TS muxer panicked"),
            _ => {}
        }
    });

    // OS thread: segment splitter — reads TS output, splits on keyframe signal
    let oq2 = output_queue.clone();
    let kf = keyframe_signal.clone();
    let st = store.clone();
    let ct2 = cancel_token.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_segment_splitter(oq2, kf, st, ct2);
        }));
        if result.is_err() {
            eprintln!("[hls] Segment splitter panicked");
        }
    });

    // Async: feed RingBuffer packets into input MemoryQueue, signal keyframes
    let mut reader = Reader::new(ring_buffer);
    let mut packets = Vec::with_capacity(32);
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
                    let mut write_start = got_first_keyframe.then_some(0);
                    for (index, packet) in packets.iter().enumerate() {
                        if packet.media_type == MediaType::Video && packet.is_keyframe {
                            if got_first_keyframe {
                                keyframe_signal.store(true, Ordering::Release);
                            }
                            got_first_keyframe = true;
                        }
                        if got_first_keyframe && write_start.is_none() {
                            write_start = Some(index);
                        }
                    }
                    if let Some(start) = write_start {
                        input_queue.write_batch(
                            packets[start..]
                                .iter()
                                .map(|packet| packet.payload.as_ref()),
                        );
                    }
                }
            }
        }
    }

    input_queue.close();
}

fn run_ts_muxer(
    in_queue: Arc<crate::media::avio::MemoryQueue>,
    out_queue: Arc<crate::media::avio::MemoryQueue>,
    token: CancellationToken,
) -> Result<(), &'static str> {
    use crate::media::avio::{CustomInput, CustomOutput};

    let mut custom_input = CustomInput::new(&*in_queue)?;
    let mut ictx = custom_input
        .input
        .take()
        .ok_or("Failed to get input context")?;

    let mut custom_output = CustomOutput::new(&*out_queue, "mpegts")?;
    let mut octx = custom_output
        .output
        .take()
        .ok_or("Failed to get output context")?;

    let mut stream_mapping = Vec::new();
    for stream in ictx.streams() {
        let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::None);
        let mut new_stream = octx
            .add_stream(codec)
            .map_err(|_| "HLS: Failed to add stream")?;
        new_stream.set_parameters(stream.parameters());
        stream_mapping.push(new_stream.index());
    }

    octx.write_header()
        .map_err(|_| "HLS: Failed to write header")?;

    for (stream, mut packet) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }

        let Some(&out_idx) = stream_mapping.get(stream.index()) else {
            continue;
        };
        packet.set_stream(out_idx);

        let in_tb = stream.time_base();
        let Some(out_stream) = octx.stream(out_idx) else {
            continue;
        };
        let out_tb = out_stream.time_base();
        packet.rescale_ts(in_tb, out_tb);

        let _ = packet.write_interleaved(&mut octx);
    }

    octx.write_trailer()
        .map_err(|_| "HLS: Write trailer failed")?;
    out_queue.close();
    Ok(())
}

fn run_segment_splitter(
    output_queue: Arc<crate::media::avio::MemoryQueue>,
    keyframe_signal: Arc<AtomicBool>,
    store: Arc<HlsStore>,
    token: CancellationToken,
) {
    let mut buf = vec![0u8; 32768];
    // 8 MB: a 4K60 H.264 segment at 6s target duration can reach 4-8 MB;
    // pre-allocating avoids repeated reallocs during the first segment.
    let mut accumulator = BytesMut::with_capacity(SEGMENT_CAPACITY);
    let mut segment_start = Instant::now();

    loop {
        if token.is_cancelled() {
            break;
        }

        let n = output_queue.read_nonblocking(&mut buf);
        if n == 0 {
            if output_queue.is_closed() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }

        accumulator.extend_from_slice(&buf[..n]);

        let elapsed = segment_start.elapsed().as_secs_f64();
        let should_split =
            keyframe_signal.swap(false, Ordering::AcqRel) && elapsed >= MIN_SEGMENT_SECS;

        if should_split && !accumulator.is_empty() {
            store.push_segment(elapsed, accumulator.split().freeze());
            accumulator.reserve(SEGMENT_CAPACITY);
            segment_start = Instant::now();
        }
    }

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
