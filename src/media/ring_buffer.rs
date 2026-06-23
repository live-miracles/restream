//! Lock-free single-producer multi-consumer ring buffer for media packet fan-out.
//!
//! # Memory Layout
//!
//! Packet slots are densely packed because readers only load them; cache-line
//! isolation is reserved for the producer-owned indexes that are actively
//! modified. This keeps the 4096-slot working set small enough for cache.
//!
//! # Packet Walk
//!
//! ```text
//! Ingest (RTMP/SRT demuxer)
//!   → push(MediaPacket)
//!     → ArcSwapOption::store() on slot[write_idx % capacity]
//!     → AtomicUsize::store(write_idx + 1, Release)
//!     → Notify::notify_waiters()
//!
//! Reader (egress / HLS / recording)
//!   → wait_for_data()  (Notify::notified().await)
//!   → pull()
//!     → ArcSwapOption::load_full() on slot[read_idx % capacity]
//!     → returns Arc<MediaPacket> (zero-copy, ref-counted)
//! ```
//!
//! # Capacity
//!
//! Default: 4096 slots. At 4K 60fps a stream produces ~120 video + ~50 audio
//! = ~170 packets/sec, giving ~24 seconds of buffer depth. At 1080p30 the
//! depth doubles. This is sufficient for transient egress stalls without
//! triggering overflow, while keeping per-pipeline memory bounded (the slots
//! hold `Arc` refs, not copies — actual payload memory is shared via refcount).
//!
//! # Overflow & Recovery
//!
//! When a reader falls behind by ≥ capacity slots, `pull()` detects the gap
//! and calls `fast_forward()`, which jumps to the most recent keyframe via
//! an O(1) atomic read of `last_keyframe_idx`. This avoids decoding artifacts
//! by always resuming from an IDR frame.
//!
//! # Why ArcSwap
//!
//! Single-writer is guaranteed by the monotonic `write_idx` — only the ingest
//! thread ever calls `push()`. Multiple readers call `load_full()` concurrently
//! without any locking. This eliminates the per-slot RwLock contention that
//! would otherwise be the bottleneck at 500+ concurrent egress readers.

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadFormat {
    Flv,
    Raw,
}

#[derive(Clone, Debug)]
pub struct MediaPacket {
    pub media_type: MediaType,
    pub track_index: u32,
    pub pts: i64,
    pub dts: i64,
    pub is_keyframe: bool,
    pub format: PayloadFormat,
    pub payload: Bytes,
}

pub struct RingSlot {
    data: ArcSwapOption<MediaPacket>,
}

#[repr(align(64))]
pub struct AlignedAtomicUsize {
    val: AtomicUsize,
}

pub struct ReaderInfo {
    pub name: String,
    pub read_idx: AtomicUsize,
    pub overflow_count: AtomicUsize,
}

pub struct RingBuffer {
    slots: Vec<RingSlot>,
    write_idx: AlignedAtomicUsize,
    last_keyframe_idx: AlignedAtomicUsize,
    capacity: usize,
    notify: Arc<tokio::sync::Notify>,
    pub readers: std::sync::Mutex<Vec<std::sync::Weak<ReaderInfo>>>,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(RingSlot {
                data: ArcSwapOption::empty(),
            });
        }
        Self {
            slots,
            write_idx: AlignedAtomicUsize {
                val: AtomicUsize::new(0),
            },
            last_keyframe_idx: AlignedAtomicUsize {
                val: AtomicUsize::new(0),
            },
            capacity,
            notify: Arc::new(tokio::sync::Notify::new()),
            readers: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, packet: MediaPacket) {
        let idx = self.write_idx.val.load(Ordering::Relaxed);
        let slot_idx = idx % self.capacity;
        let is_keyframe = packet.media_type == MediaType::Video && packet.is_keyframe;

        self.slots[slot_idx].data.store(Some(Arc::new(packet)));

        if is_keyframe {
            self.last_keyframe_idx.val.store(idx, Ordering::Release);
        }

        self.write_idx.val.store(idx + 1, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Publish a burst with one write-index release and one waiter notification.
    ///
    /// The ring is single-producer, so slots can be populated first and made
    /// visible together by the final release store. Returns the number of
    /// packets published.
    pub fn push_batch<I>(&self, packets: I) -> usize
    where
        I: IntoIterator<Item = MediaPacket>,
    {
        let start_idx = self.write_idx.val.load(Ordering::Relaxed);
        let mut count = 0usize;

        for packet in packets {
            let idx = start_idx + count;
            let slot_idx = idx % self.capacity;
            let is_keyframe = packet.media_type == MediaType::Video && packet.is_keyframe;

            self.slots[slot_idx].data.store(Some(Arc::new(packet)));
            if is_keyframe {
                self.last_keyframe_idx.val.store(idx, Ordering::Release);
            }
            count += 1;
        }

        if count > 0 {
            self.write_idx
                .val
                .store(start_idx + count, Ordering::Release);
            self.notify.notify_waiters();
        }

        count
    }

    pub fn read_at(&self, idx: usize) -> Option<Arc<MediaPacket>> {
        let slot_idx = idx % self.capacity;
        self.slots[slot_idx].data.load_full()
    }

    pub fn get_write_idx(&self) -> usize {
        self.write_idx.val.load(Ordering::Acquire)
    }

    pub fn get_notify(&self) -> Arc<tokio::sync::Notify> {
        self.notify.clone()
    }

    pub fn fill_and_capacity(&self) -> (usize, usize) {
        let write_idx = self.write_idx.val.load(Ordering::Relaxed);
        let fill = write_idx.min(self.capacity);
        (fill, self.capacity)
    }

    pub fn fast_forward(&self, current_write_idx: usize) -> usize {
        let kf_idx = self.last_keyframe_idx.val.load(Ordering::Acquire);
        if kf_idx > 0 && current_write_idx.saturating_sub(kf_idx) < self.capacity {
            return kf_idx;
        }
        current_write_idx.saturating_sub(100)
    }
}

pub struct Reader {
    buffer: Arc<RingBuffer>,
    pub info: Arc<ReaderInfo>,
    read_idx: usize,
}

impl Drop for Reader {
    fn drop(&mut self) {
        // Remove our entry and any other stale Weak refs from the ring's reader
        // list.  Called while self.info still has strong_count = 1 (our field),
        // so we use Arc::ptr_eq to identify our slot; entries where upgrade()
        // returns None are also pruned.
        if let Ok(mut readers) = self.buffer.readers.lock() {
            readers.retain(|w| match w.upgrade() {
                Some(info) => !Arc::ptr_eq(&info, &self.info),
                None => false,
            });
        }
    }
}

impl Reader {
    pub fn new(name: String, buffer: Arc<RingBuffer>) -> Self {
        let current_write = buffer.get_write_idx();
        let start_idx = buffer.fast_forward(current_write);
        let info = Arc::new(ReaderInfo {
            name,
            read_idx: AtomicUsize::new(start_idx),
            overflow_count: AtomicUsize::new(0),
        });

        {
            let mut r = buffer.readers.lock().unwrap();
            r.push(Arc::downgrade(&info));
        }

        Self {
            buffer,
            info,
            read_idx: start_idx,
        }
    }

    pub fn pull(&mut self) -> Result<Option<Arc<MediaPacket>>, &'static str> {
        let write_idx = self.buffer.get_write_idx();

        if write_idx > self.read_idx && write_idx - self.read_idx >= self.buffer.capacity {
            let new_idx = self.buffer.fast_forward(write_idx);
            self.read_idx = new_idx;
            self.info.read_idx.store(new_idx, Ordering::Relaxed);
            self.info.overflow_count.fetch_add(1, Ordering::Relaxed);
            return Err("Overflow: reader lagged and was fast-forwarded");
        }

        if self.read_idx == write_idx {
            return Ok(None);
        }

        let packet = self.buffer.read_at(self.read_idx);
        if packet.is_some() {
            self.read_idx += 1;
            self.info.read_idx.store(self.read_idx, Ordering::Relaxed);
        }
        Ok(packet)
    }

    /// Load up to `max_packets` using one write-index acquisition.
    ///
    /// Appends packets to `output` and returns the number appended. Overflow
    /// behavior matches `pull()`.
    pub fn pull_burst(
        &mut self,
        output: &mut Vec<Arc<MediaPacket>>,
        max_packets: usize,
    ) -> Result<usize, &'static str> {
        if max_packets == 0 {
            return Ok(0);
        }

        let write_idx = self.buffer.get_write_idx();
        if write_idx > self.read_idx && write_idx - self.read_idx >= self.buffer.capacity {
            self.read_idx = self.buffer.fast_forward(write_idx);
            self.info.read_idx.store(self.read_idx, Ordering::Relaxed);
            self.info.overflow_count.fetch_add(1, Ordering::Relaxed);
            return Err("Overflow: reader lagged and was fast-forwarded");
        }

        let available = write_idx.saturating_sub(self.read_idx).min(max_packets);
        output.reserve(available);
        let start_len = output.len();

        for idx in self.read_idx..self.read_idx + available {
            let Some(packet) = self.buffer.read_at(idx) else {
                break;
            };
            output.push(packet);
        }

        let loaded = output.len() - start_len;
        self.read_idx += loaded;
        self.info.read_idx.store(self.read_idx, Ordering::Relaxed);
        Ok(loaded)
    }

    pub async fn wait_for_data(&self) {
        let notify = self.buffer.get_notify();
        loop {
            // Re-check for data before blocking to avoid a TOCTOU race:
            // the writer could notify_waiters() between our pull() returning
            // None and this notified().await registering — Notify does NOT
            // store notifications for future waiters, so we'd sleep forever.
            if self.buffer.get_write_idx() > self.read_idx {
                return;
            }
            notify.notified().await;
        }
    }
}

/// Per-stream DTS monotonicity enforcer for MPEG-TS muxing.
///
/// FFmpeg's `write_interleaved` requires strictly increasing DTS per stream.
/// Audio packets at millisecond granularity can share timestamps (e.g. two AAC
/// frames in the same millisecond). This enforcer bumps colliding DTS by 1 and
/// adjusts PTS to maintain PTS >= DTS.
pub struct DtsEnforcer {
    last_dts: Vec<i64>,
}

impl DtsEnforcer {
    pub fn new(num_streams: usize) -> Self {
        Self {
            last_dts: vec![i64::MIN; num_streams],
        }
    }

    /// Enforce monotonically increasing DTS for a given stream.
    /// Returns the corrected (pts, dts) pair.
    pub fn enforce(&mut self, stream_idx: usize, pts: i64, dts: i64) -> (i64, i64) {
        let mut dts = dts;
        if let Some(prev) = self.last_dts.get(stream_idx)
            && dts <= *prev
        {
            dts = *prev + 1;
        }
        let pts = if pts < dts { dts } else { pts };
        if let Some(slot) = self.last_dts.get_mut(stream_idx) {
            *slot = dts;
        }
        (pts, dts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn video_packet(pts: i64, dts: i64, keyframe: bool) -> MediaPacket {
        MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts,
            dts,
            is_keyframe: keyframe,
            format: PayloadFormat::Raw,
            payload: Bytes::from_static(&[0; 16]),
        }
    }

    fn audio_packet(pts: i64, dts: i64) -> MediaPacket {
        MediaPacket {
            media_type: MediaType::Audio,
            track_index: 0,
            pts,
            dts,
            is_keyframe: false,
            format: PayloadFormat::Raw,
            payload: Bytes::from_static(&[0; 4]),
        }
    }

    // -- RingBuffer push/pull --

    #[test]
    fn push_then_pull_returns_packets_in_order() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let rb = Arc::new(RingBuffer::new(16));
            rb.push(video_packet(0, 0, true));
            rb.push(audio_packet(10, 10));
            rb.push(video_packet(33, 30, false));

            let mut reader = Reader::new("test".to_string(), rb);
            let p1 = reader.pull().unwrap().unwrap();
            assert_eq!(p1.pts, 0);
            assert!(p1.is_keyframe);

            let p2 = reader.pull().unwrap().unwrap();
            assert_eq!(p2.media_type, MediaType::Audio);
            assert_eq!(p2.pts, 10);

            let p3 = reader.pull().unwrap().unwrap();
            assert_eq!(p3.pts, 33);

            assert!(reader.pull().unwrap().is_none());
        });
    }

    #[test]
    fn push_batch_then_pull_burst_returns_packets_in_order() {
        let ring = Arc::new(RingBuffer::new(16));
        let published = ring.push_batch([
            video_packet(10, 10, true),
            video_packet(20, 20, false),
            video_packet(30, 30, false),
        ]);
        assert_eq!(published, 3);
        assert_eq!(ring.get_write_idx(), 3);

        let mut reader = Reader::new("test_burst".to_string(), ring);
        let mut packets = Vec::new();
        assert_eq!(reader.pull_burst(&mut packets, 2).unwrap(), 2);
        assert_eq!(reader.pull_burst(&mut packets, 2).unwrap(), 1);
        assert_eq!(
            packets.iter().map(|packet| packet.pts).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn empty_batch_does_not_advance_ring() {
        let ring = RingBuffer::new(16);
        assert_eq!(ring.push_batch(std::iter::empty()), 0);
        assert_eq!(ring.get_write_idx(), 0);
    }

    #[test]
    fn reader_starts_at_last_keyframe() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let rb = Arc::new(RingBuffer::new(64));
            // Push some packets, including keyframes at different positions
            for i in 0..20 {
                rb.push(video_packet(i * 33, i * 33, i % 10 == 0)); // KF at 0, 10
            }
            rb.push(audio_packet(660, 660));

            let mut reader = Reader::new("test_starts".to_string(), rb);
            // Should start at or after the last keyframe (index 10)
            let first = reader.pull().unwrap().unwrap();
            assert!(first.pts >= 10 * 33);
        });
    }

    #[test]
    fn overflow_triggers_fast_forward_to_keyframe() {
        let rb = Arc::new(RingBuffer::new(8));

        let info = Arc::new(ReaderInfo {
            name: "test_overflow".to_string(),
            read_idx: AtomicUsize::new(0),
            overflow_count: AtomicUsize::new(0),
        });
        let mut reader = Reader {
            buffer: rb.clone(),
            info,
            read_idx: 0,
        };

        // Push 20 packets with a keyframe at index 15
        for i in 0..20 {
            rb.push(video_packet(i * 33, i * 33, i == 0 || i == 15));
        }
        // write_idx=20, reader at 0, gap=20 >= capacity=8 → overflow

        let result = reader.pull();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Overflow"));
        assert_eq!(reader.info.overflow_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn multiple_readers_pull_same_packets() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let rb = Arc::new(RingBuffer::new(64));
            rb.push(video_packet(0, 0, true));
            rb.push(video_packet(33, 33, false));

            let mut r1 = Reader::new("r1".to_string(), rb.clone());
            let mut r2 = Reader::new("r2".to_string(), rb.clone());

            let p1 = r1.pull().unwrap().unwrap();
            let p2 = r2.pull().unwrap().unwrap();
            assert_eq!(p1.pts, p2.pts);
            assert_eq!(p1.dts, p2.dts);
        });
    }

    #[test]
    fn fill_and_capacity_reports_correct_values() {
        let rb = RingBuffer::new(16);
        assert_eq!(rb.fill_and_capacity(), (0, 16));

        rb.push(video_packet(0, 0, true));
        assert_eq!(rb.fill_and_capacity(), (1, 16));

        for i in 1..16 {
            rb.push(audio_packet(i, i));
        }
        assert_eq!(rb.fill_and_capacity(), (16, 16));

        // After wrapping, fill stays at capacity
        rb.push(audio_packet(100, 100));
        assert_eq!(rb.fill_and_capacity(), (16, 16));
    }

    // -- DtsEnforcer --

    #[test]
    fn dts_enforcer_passes_through_increasing_dts() {
        let mut e = DtsEnforcer::new(2);
        assert_eq!(e.enforce(0, 0, 0), (0, 0));
        assert_eq!(e.enforce(0, 33, 33), (33, 33));
        assert_eq!(e.enforce(0, 66, 66), (66, 66));
    }

    #[test]
    fn dts_enforcer_bumps_equal_dts() {
        let mut e = DtsEnforcer::new(2);
        // Two audio packets with the same DTS (common at ms granularity)
        assert_eq!(e.enforce(1, 10, 10), (10, 10));
        assert_eq!(e.enforce(1, 10, 10), (11, 11)); // bumped
        assert_eq!(e.enforce(1, 10, 10), (12, 12)); // bumped again
    }

    #[test]
    fn dts_enforcer_bumps_decreasing_dts() {
        let mut e = DtsEnforcer::new(1);
        assert_eq!(e.enforce(0, 100, 100), (100, 100));
        assert_eq!(e.enforce(0, 50, 50), (101, 101)); // backwards jump corrected
    }

    #[test]
    fn dts_enforcer_adjusts_pts_below_dts() {
        let mut e = DtsEnforcer::new(1);
        assert_eq!(e.enforce(0, 100, 100), (100, 100));
        // PTS=90, DTS=90 → DTS bumped to 101, PTS raised to 101
        assert_eq!(e.enforce(0, 90, 90), (101, 101));
    }

    #[test]
    fn dts_enforcer_preserves_pts_cts_offset() {
        let mut e = DtsEnforcer::new(1);
        // B-frame pattern: PTS ahead of DTS (composition time offset)
        assert_eq!(e.enforce(0, 132, 99), (132, 99));
        assert_eq!(e.enforce(0, 165, 132), (165, 132));
    }

    #[test]
    fn dts_enforcer_independent_per_stream() {
        let mut e = DtsEnforcer::new(2);
        assert_eq!(e.enforce(0, 100, 100), (100, 100));
        // Stream 1 has its own DTS tracking
        assert_eq!(e.enforce(1, 50, 50), (50, 50));
        // Stream 0 continues from 100
        assert_eq!(e.enforce(0, 100, 100), (101, 101));
    }

    #[test]
    fn dts_enforcer_handles_out_of_bounds_stream() {
        let mut e = DtsEnforcer::new(1);
        // Stream index 5 is out of bounds — passes through unchanged
        assert_eq!(e.enforce(5, 100, 100), (100, 100));
    }

    // -- Reader::drop lifecycle --

    #[test]
    fn reader_drop_removes_entry_from_readers_list() {
        let rb = Arc::new(RingBuffer::new(16));

        assert_eq!(rb.readers.lock().unwrap().len(), 0);

        let r1 = Reader::new("r1".into(), rb.clone());
        let r2 = Reader::new("r2".into(), rb.clone());
        assert_eq!(rb.readers.lock().unwrap().len(), 2);

        drop(r1);
        // After drop, our entry is removed and no stale Weak remains.
        assert_eq!(rb.readers.lock().unwrap().len(), 1);

        drop(r2);
        assert_eq!(rb.readers.lock().unwrap().len(), 0);
    }

    #[test]
    fn reader_drop_also_prunes_other_stale_weaks() {
        // Simulate a stale Weak that was left behind (e.g. from a previous bug)
        // by manually inserting one, then verifying drop cleans it.
        let rb = Arc::new(RingBuffer::new(16));
        {
            // Insert a Weak that immediately becomes stale.
            let ephemeral = Arc::new(ReaderInfo {
                name: "stale".into(),
                read_idx: AtomicUsize::new(0),
                overflow_count: AtomicUsize::new(0),
            });
            rb.readers.lock().unwrap().push(Arc::downgrade(&ephemeral));
            // ephemeral drops here → Weak becomes stale
        }
        assert_eq!(rb.readers.lock().unwrap().len(), 1); // stale entry present

        let r = Reader::new("live".into(), rb.clone());
        assert_eq!(rb.readers.lock().unwrap().len(), 2); // stale + live

        drop(r);
        // drop() removes our entry AND prunes the stale one.
        assert_eq!(rb.readers.lock().unwrap().len(), 0);
    }
}
