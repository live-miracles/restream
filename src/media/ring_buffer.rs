//! Lock-free single-producer multi-consumer ring buffer for media packet fan-out.
//!
//! # Memory Layout
//!
//! Packet slots are densely packed because readers only load them; cache-line
//! isolation is reserved for the producer-owned indexes that are actively
//! modified. This keeps the slot working set small enough for cache.
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
//! Default: 1024 slots, configurable with `RESTREAM_RING_CAPACITY`. Slots hold
//! demuxed media packets rather than fixed 188-byte TS packets, so retained
//! payload memory scales with compressed frame size. The default gives tens of
//! seconds of burst tolerance for common live inputs while bounding memory
//! earlier than the old 4096-slot default.
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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

pub const DEFAULT_RING_CAPACITY: usize = 1024;
const MIN_RING_CAPACITY: usize = 64;
const MAX_RING_CAPACITY: usize = 16_384;
static RING_CAPACITY: OnceLock<usize> = OnceLock::new();

pub fn default_ring_capacity() -> usize {
    *RING_CAPACITY.get_or_init(|| {
        std::env::var("RESTREAM_RING_CAPACITY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_RING_CAPACITY)
            .clamp(MIN_RING_CAPACITY, MAX_RING_CAPACITY)
    })
}

// Transcoder output rings hold demuxed frames from the FFmpeg child process.
// At 720p30 with one audio track, the packet rate is ~80 pkt/s; 512 slots
// ≈ 6.4 s of jitter headroom (above the 5 s requirement). I-frames from the
// CRF23 encoder are large (~30–50 KB each), so the per-slot payload size is
// much larger than the source ring — 512 slots already dominate memory at high
// bitrates. Scale-test evidence: no transcoder ring overflows across 15 cases.
pub const DEFAULT_TRANSCODER_RING_CAPACITY: usize = 512;
static TRANSCODER_RING_CAPACITY: OnceLock<usize> = OnceLock::new();

pub fn default_transcoder_ring_capacity() -> usize {
    *TRANSCODER_RING_CAPACITY.get_or_init(|| {
        std::env::var("RESTREAM_TRANSCODER_RING_CAPACITY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_TRANSCODER_RING_CAPACITY)
            .clamp(MIN_RING_CAPACITY, MAX_RING_CAPACITY)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaType {
    Video = 0,
    Audio = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PayloadFormat {
    Flv = 0,
    Raw = 1,
}

/// 56-byte media packet.  `#[repr(C)]` pins the field order so the declared
/// layout is always respected, preventing the compiler from reordering fields
/// into a layout that scatters hot fields across two cache lines.
///
/// Without `#[repr(C)]`, rustc's default greedy-alignment algorithm places the
/// largest field (`payload: Bytes`, 32 bytes) first within the struct.  That
/// puts `media_type`, `is_keyframe`, and `pts`/`dts` at offsets 52–63 inside
/// `ArcInner`, spanning two 64-byte cache lines — reading `media_type` to
/// dispatch the packet requires the *second* cache line.
///
/// With the declared field order the `ArcInner<MediaPacket>` layout is:
/// ```text
/// Byte  0– 7  strong refcount          (ArcInner header)
/// Byte  8–15  weak refcount            (ArcInner header)
/// Byte 16     media_type               ← cache line 0 (bytes 0–63)
/// Byte 17     format
/// Byte 18     is_keyframe
/// Byte 19     (1 byte padding)
/// Byte 20–23  track_index
/// Byte 24–31  pts
/// Byte 32–39  dts
/// Byte 40–47  payload.ptr              ← pointer to codec output, needed immediately
/// Byte 48–55  payload.len
/// Byte 56–63  payload.data             ← cache line 1 (bytes 64+): Arc management only
/// Byte 64–71  payload.vtable
/// ```
///
/// All hot consumer fields — type dispatch, track routing, timestamps, and the
/// payload pointer+length — fit in the first cache line.  Only the `Bytes` Arc
/// management fields (`data`, `vtable`) land in the second cache line, and those
/// are only touched on clone/drop, not on every field read.
///
/// Access groups ordered by first-touch in the hot path:
///   1. Type dispatch  : `media_type`, `format`, `is_keyframe` (offset  0– 2)
///   2. Track routing  : `track_index`                          (offset  4– 7)
///   3. Timestamps     : `pts`, `dts`                          (offset  8–23)
///   4. Payload        : `payload`                             (offset 24–55)
#[derive(Clone, Debug)]
#[repr(C)]
pub struct MediaPacket {
    // Group 1: type dispatch — read first in every consumer (3 bytes + 1 pad = 4)
    pub media_type: MediaType,
    pub format: PayloadFormat,
    pub is_keyframe: bool,
    // 1 byte implicit C padding before the u32
    // Group 2: track routing
    pub track_index: u32,
    // Group 3: timestamps — DTS enforcer reads both together
    pub pts: i64,
    pub dts: i64,
    // Group 4: payload — largest field, accessed after codec dispatch
    pub payload: Bytes,
}

pub struct RingSlot {
    data: ArcSwapOption<MediaPacket>,
    published_at_us: AtomicU64,
}

#[repr(align(64))]
pub struct AlignedAtomicUsize {
    val: AtomicUsize,
}

/// Compact histogram for `pull_burst` yield sizes.
///
/// Buckets cover [1], [2], [3-4], [5-8], [9-16], [17-32].
/// Burst size 0 (nothing available) is not counted — callers skip stat
/// recording when `available == 0`.
pub const BURST_HIST_BUCKETS: usize = 6;
const fn burst_bucket(n: usize) -> usize {
    match n {
        1 => 0,
        2 => 1,
        3..=4 => 2,
        5..=8 => 3,
        9..=16 => 4,
        _ => 5,
    }
}

pub struct ReaderInfo {
    pub name: String,
    pub read_idx: AtomicUsize,
    pub overflow_count: AtomicUsize,
    /// Total `pull_burst` calls that returned ≥ 1 packet.
    pub burst_count: AtomicU64,
    /// Total packets returned across all bursts (avg = packet_sum / burst_count).
    pub packet_sum: AtomicU64,
    /// Histogram of burst sizes across 6 buckets (see `burst_bucket`).
    pub burst_hist: [AtomicU64; BURST_HIST_BUCKETS],
}

#[derive(Debug, Clone)]
pub struct ReaderSnapshot {
    pub name: String,
    pub read_idx: usize,
    pub write_idx: usize,
    pub lag_slots: usize,
    pub overflow_count: usize,
    pub packet_age_ms: Option<u64>,
    pub burst_count: u64,
    pub avg_burst_size: f64,
    pub median_burst_size: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PayloadStats {
    pub slots: usize,
    pub payload_bytes: usize,
    pub video_bytes: usize,
    pub audio_bytes: usize,
    pub min_payload_bytes: usize,
    pub max_payload_bytes: usize,
}

impl ReaderInfo {
    fn new(name: String, read_idx: usize) -> Self {
        Self {
            name,
            read_idx: AtomicUsize::new(read_idx),
            overflow_count: AtomicUsize::new(0),
            burst_count: AtomicU64::new(0),
            packet_sum: AtomicU64::new(0),
            burst_hist: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Snapshot of burst size statistics: (avg, approx_median, burst_count).
    pub fn burst_stats(&self) -> (f64, usize, u64) {
        let bursts = self.burst_count.load(Ordering::Relaxed);
        let pkts = self.packet_sum.load(Ordering::Relaxed);
        let avg = if bursts > 0 {
            pkts as f64 / bursts as f64
        } else {
            0.0
        };

        // Approximate median: walk histogram buckets until cumulative count ≥ 50%
        let hist: [u64; BURST_HIST_BUCKETS] =
            std::array::from_fn(|i| self.burst_hist[i].load(Ordering::Relaxed));
        let median = {
            let half = bursts.div_ceil(2);
            let mut cum = 0u64;
            let mut median_bucket = 0usize;
            for (i, &count) in hist.iter().enumerate() {
                cum += count;
                if cum >= half {
                    median_bucket = i;
                    break;
                }
            }
            // Return representative value for the bucket midpoint
            match median_bucket {
                0 => 1,
                1 => 2,
                2 => 3,
                3 => 6,
                4 => 12,
                _ => 24,
            }
        };
        (avg, median, bursts)
    }
}

pub struct RingBuffer {
    slots: Vec<RingSlot>,
    write_idx: AlignedAtomicUsize,
    last_keyframe_idx: AlignedAtomicUsize,
    capacity: usize,
    created_at: Instant,
    notify: Arc<tokio::sync::Notify>,
    pub readers: std::sync::Mutex<Vec<std::sync::Weak<ReaderInfo>>>,
    /// Video codec of packets in this ring, set once by the producer.
    /// `"h264"`, `"hevc"`, or empty string (= infer from ingest metadata).
    /// All packets in a ring share one codec — this avoids per-packet tagging.
    pub codec_hint: std::sync::OnceLock<String>,
    /// Audio tracks metadata of packets in this ring.
    pub audio_tracks: std::sync::OnceLock<Vec<crate::media::engine::AudioMeta>>,
    /// Estimated packet rate (pkt/s) set once after stream probe.
    /// Used by telemetry to compute buffer depth in seconds.
    pub estimated_pkt_rate: std::sync::atomic::AtomicU32,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(RingSlot {
                data: ArcSwapOption::empty(),
                published_at_us: AtomicU64::new(0),
            });
        }
        Self {
            slots,
            write_idx: AlignedAtomicUsize {
                val: AtomicUsize::new(0),
            },
            last_keyframe_idx: AlignedAtomicUsize {
                // usize::MAX is the sentinel meaning "no keyframe seen yet".
                // This disambiguates from a real keyframe at slot 0 (which
                // would also produce index 0 if we started from AtomicUsize::new(0)).
                val: AtomicUsize::new(usize::MAX),
            },
            capacity,
            created_at: Instant::now(),
            notify: Arc::new(tokio::sync::Notify::new()),
            readers: std::sync::Mutex::new(Vec::new()),
            codec_hint: std::sync::OnceLock::new(),
            audio_tracks: std::sync::OnceLock::new(),
            estimated_pkt_rate: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of readers whose `Arc<ReaderInfo>` is still alive.
    pub fn active_reader_count(&self) -> usize {
        self.readers
            .lock()
            .map(|mut g| {
                g.retain(|w| w.upgrade().is_some());
                g.len()
            })
            .unwrap_or(0)
    }

    /// Store the probed packet rate so telemetry can show buffer depth in seconds.
    pub fn set_estimated_pkt_rate(&self, pkt_per_sec: f64) {
        self.estimated_pkt_rate
            .store(pkt_per_sec.round() as u32, Ordering::Relaxed);
    }

    /// Buffer depth in seconds: how long the ring can absorb an ingest interruption.
    /// Returns `None` if the packet rate hasn't been set yet.
    pub fn buffer_depth_secs(&self) -> Option<f64> {
        let rate = self.estimated_pkt_rate.load(Ordering::Relaxed);
        if rate == 0 {
            return None;
        }
        Some(self.capacity as f64 / rate as f64)
    }

    /// Set the video codec hint for this ring.  Called once by the producer
    /// (e.g. external transcoder, hevc_to_h264 stage).  No-op if already set.
    pub fn set_codec_hint(&self, codec: &str) {
        let _ = self.codec_hint.set(codec.to_string());
    }

    /// Return the codec hint if set, or empty string.
    pub fn codec_hint_str(&self) -> &str {
        self.codec_hint.get().map(|s| s.as_str()).unwrap_or("")
    }

    pub fn set_audio_tracks(&self, tracks: Vec<crate::media::engine::AudioMeta>) {
        let _ = self.audio_tracks.set(tracks);
    }

    pub fn audio_tracks(&self) -> Option<&[crate::media::engine::AudioMeta]> {
        self.audio_tracks.get().map(|v| v.as_slice())
    }

    pub fn push(&self, packet: MediaPacket) {
        let idx = self.write_idx.val.load(Ordering::Relaxed);
        let slot_idx = idx % self.capacity;
        let is_keyframe = packet.media_type == MediaType::Video && packet.is_keyframe;

        self.slots[slot_idx]
            .published_at_us
            .store(self.elapsed_us().max(1), Ordering::Release);
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

            self.slots[slot_idx]
                .published_at_us
                .store(self.elapsed_us().max(1), Ordering::Release);
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

    fn elapsed_us(&self) -> u64 {
        self.created_at.elapsed().as_micros().min(u64::MAX as u128) as u64
    }

    pub fn get_write_idx(&self) -> usize {
        self.write_idx.val.load(Ordering::Acquire)
    }

    pub fn get_notify(&self) -> Arc<tokio::sync::Notify> {
        self.notify.clone()
    }

    pub fn min_read_idx(&self) -> usize {
        let write_idx = self.write_idx.val.load(Ordering::Relaxed);
        if let Ok(readers) = self.readers.lock() {
            let mut min_idx = write_idx;
            let mut has_readers = false;
            for w in readers.iter() {
                if let Some(info) = w.upgrade() {
                    let r_idx = info.read_idx.load(Ordering::Relaxed);
                    min_idx = min_idx.min(r_idx);
                    has_readers = true;
                }
            }
            if has_readers { min_idx } else { write_idx }
        } else {
            write_idx
        }
    }

    pub fn fill_and_capacity(&self) -> (usize, usize) {
        let write_idx = self.write_idx.val.load(Ordering::Relaxed);
        if let Ok(readers) = self.readers.lock() {
            let mut min_idx = write_idx;
            let mut has_readers = false;
            for w in readers.iter() {
                if let Some(info) = w.upgrade() {
                    let r_idx = info.read_idx.load(Ordering::Relaxed);
                    min_idx = min_idx.min(r_idx);
                    has_readers = true;
                }
            }
            let fill = if has_readers {
                write_idx.saturating_sub(min_idx).min(self.capacity)
            } else {
                write_idx.min(self.capacity)
            };
            (fill, self.capacity)
        } else {
            (write_idx.min(self.capacity), self.capacity)
        }
    }

    pub fn payload_stats(&self) -> PayloadStats {
        let mut stats = PayloadStats::default();
        let mut min_payload = usize::MAX;

        for slot in &self.slots {
            let Some(packet) = slot.data.load_full() else {
                continue;
            };
            let len = packet.payload.len();
            stats.slots += 1;
            stats.payload_bytes = stats.payload_bytes.saturating_add(len);
            min_payload = min_payload.min(len);
            stats.max_payload_bytes = stats.max_payload_bytes.max(len);
            match packet.media_type {
                MediaType::Video => {
                    stats.video_bytes = stats.video_bytes.saturating_add(len);
                }
                MediaType::Audio => {
                    stats.audio_bytes = stats.audio_bytes.saturating_add(len);
                }
            }
        }

        if stats.slots > 0 {
            stats.min_payload_bytes = min_payload;
        }

        stats
    }

    pub fn reader_snapshots(&self) -> Vec<ReaderSnapshot> {
        let write_idx = self.get_write_idx();
        let now_us = self.elapsed_us();
        let mut snapshots = Vec::new();

        let mut readers = self.readers.lock().unwrap_or_else(|e| e.into_inner());
        readers.retain(|weak_ref| {
            let Some(info) = weak_ref.upgrade() else {
                return false;
            };

            let read_idx = info.read_idx.load(Ordering::Acquire);
            let lag_slots = write_idx.saturating_sub(read_idx);
            let packet_age_ms = if lag_slots == 0 || lag_slots >= self.capacity {
                None
            } else {
                let slot = &self.slots[read_idx % self.capacity];
                if slot.data.load_full().is_some() {
                    let published_at_us = slot.published_at_us.load(Ordering::Acquire);
                    (published_at_us > 0).then(|| now_us.saturating_sub(published_at_us) / 1000)
                } else {
                    None
                }
            };
            let (avg_burst_size, median_burst_size, burst_count) = info.burst_stats();

            snapshots.push(ReaderSnapshot {
                name: info.name.clone(),
                read_idx,
                write_idx,
                lag_slots,
                overflow_count: info.overflow_count.load(Ordering::Relaxed),
                packet_age_ms,
                burst_count,
                avg_burst_size,
                median_burst_size,
            });

            true
        });

        snapshots
    }

    pub fn fast_forward(&self, current_write_idx: usize) -> usize {
        let kf_idx = self.last_keyframe_idx.val.load(Ordering::Acquire);
        // usize::MAX is the sentinel for "no keyframe seen yet".
        let kf_known = kf_idx != usize::MAX;
        if kf_known && current_write_idx.saturating_sub(kf_idx) < self.capacity {
            return kf_idx;
        }
        // No valid keyframe is known yet (stream start) or the last keyframe
        // index is more than `capacity` slots behind the write cursor (overflow
        // without a keyframe in the window).
        // Return the current write position to start at the live edge rather
        // than using saturating_sub(100) which returns 0 when write_idx < 100.
        current_write_idx
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
        //
        // unwrap_or_else instead of if-let-Ok: a poisoned mutex (from a panic
        // while holding the lock) must not silently skip cleanup — leaving our
        // Weak in the list would artificially inflate min_read_idx and stall
        // producer overflow recovery.
        let mut readers = self
            .buffer
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        readers.retain(|w| match w.upgrade() {
            Some(info) => !Arc::ptr_eq(&info, &self.info),
            None => false,
        });
    }
}

impl Reader {
    pub fn new(name: String, buffer: Arc<RingBuffer>) -> Self {
        let current_write = buffer.get_write_idx();
        let start_idx = buffer.fast_forward(current_write);
        let info = Arc::new(ReaderInfo::new(name, start_idx));

        {
            let mut r = buffer.readers.lock().unwrap_or_else(|e| e.into_inner());
            r.push(Arc::downgrade(&info));
        }

        Self {
            buffer,
            info,
            read_idx: start_idx,
        }
    }

    pub fn new_live(name: String, buffer: Arc<RingBuffer>) -> Self {
        let current_write = buffer.get_write_idx();
        let info = Arc::new(ReaderInfo::new(name, current_write));

        {
            let mut r = buffer.readers.lock().unwrap_or_else(|e| e.into_inner());
            r.push(Arc::downgrade(&info));
        }

        Self {
            buffer,
            info,
            read_idx: current_write,
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
        let post_write_idx = self.buffer.get_write_idx();
        if post_write_idx > self.read_idx && post_write_idx - self.read_idx >= self.buffer.capacity
        {
            let new_idx = self.buffer.fast_forward(post_write_idx);
            self.read_idx = new_idx;
            self.info.read_idx.store(new_idx, Ordering::Relaxed);
            self.info.overflow_count.fetch_add(1, Ordering::Relaxed);
            return Err("Overflow: reader lagged and was fast-forwarded");
        }

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

        let post_write_idx = self.buffer.get_write_idx();
        if post_write_idx > self.read_idx && post_write_idx - self.read_idx >= self.buffer.capacity
        {
            output.truncate(start_len);
            self.read_idx = self.buffer.fast_forward(post_write_idx);
            self.info.read_idx.store(self.read_idx, Ordering::Relaxed);
            self.info.overflow_count.fetch_add(1, Ordering::Relaxed);
            return Err("Overflow: reader lagged and was fast-forwarded");
        }

        let loaded = output.len() - start_len;
        self.read_idx += loaded;
        self.info.read_idx.store(self.read_idx, Ordering::Relaxed);
        if loaded > 0 {
            self.info.burst_count.fetch_add(1, Ordering::Relaxed);
            self.info
                .packet_sum
                .fetch_add(loaded as u64, Ordering::Relaxed);
            self.info.burst_hist[burst_bucket(loaded)].fetch_add(1, Ordering::Relaxed);
        }
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

    /// Number of slots this reader is behind the write cursor.
    ///
    /// Zero means fully caught up; values approaching `capacity` mean
    /// the reader is at risk of overflow. Useful as a health metric for slow
    /// egress consumers.
    pub fn lag(&self) -> usize {
        self.buffer.get_write_idx().saturating_sub(self.read_idx)
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

        let info = Arc::new(ReaderInfo::new("test_overflow".to_string(), 0));
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

    #[test]
    fn reader_snapshots_report_lag_overflow_and_packet_age() {
        let rb = Arc::new(RingBuffer::new(4));
        rb.push(video_packet(0, 0, true));
        let mut reader = Reader::new("slow-reader".to_string(), rb.clone());

        std::thread::sleep(std::time::Duration::from_millis(2));
        rb.push(audio_packet(10, 10));
        rb.push(audio_packet(20, 20));

        let snapshots = rb.reader_snapshots();
        assert_eq!(snapshots.len(), 1);
        let snapshot = &snapshots[0];
        assert_eq!(snapshot.name, "slow-reader");
        assert_eq!(snapshot.lag_slots, 3);
        assert_eq!(snapshot.overflow_count, 0);
        assert!(
            snapshot.packet_age_ms.is_some(),
            "lagging reader should report the age of its next unread packet"
        );

        for i in 3..8 {
            rb.push(audio_packet(i * 10, i * 10));
        }
        assert!(reader.pull().is_err());

        let snapshots = rb.reader_snapshots();
        assert_eq!(snapshots[0].overflow_count, 1);
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

    #[test]
    fn dts_enforcer_stream_idx_collision_corrupts_video_dts() {
        // Regression for issue #2: before the fix, audio packets with an
        // unknown track_index were routed to stream_idx=0 via `.unwrap_or(0)`,
        // aliasing into the video DTS slot. This test documents the corruption
        // pattern. The fix is `None => continue` in all pipeline mux loops so
        // unknown-track audio packets are dropped instead of aliased.
        let mut e = DtsEnforcer::new(2); // stream 0 = video, stream 1 = audio

        // Normal video frame.
        assert_eq!(e.enforce(0, 100, 100), (100, 100));

        // Simulate the OLD bug: an audio packet with unknown track_index is
        // incorrectly routed to stream_idx=0 (video's slot) and carries a
        // large DTS, advancing video's monotonic counter to 300.
        assert_eq!(e.enforce(0, 300, 300), (300, 300));

        // Next genuine video frame at dts=200 is now bumped to 301 instead of
        // passing through at 200, breaking A/V sync. With `None => continue`
        // the audio packet is skipped so the video counter stays at 100 and
        // dts=200 passes through correctly.
        let (_, corrupted) = e.enforce(0, 200, 200);
        assert_eq!(
            corrupted, 301,
            "aliasing audio to stream_idx=0 bumps video DTS past the actual \
             video timestamp, demonstrating the corruption fixed by None=>continue"
        );
    }

    // -- Reader::drop lifecycle --

    #[test]
    fn reader_drop_removes_entry_from_readers_list() {
        let rb = Arc::new(RingBuffer::new(16));

        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            0
        );

        let r1 = Reader::new("r1".into(), rb.clone());
        let r2 = Reader::new("r2".into(), rb.clone());
        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            2
        );

        drop(r1);
        // After drop, our entry is removed and no stale Weak remains.
        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1
        );

        drop(r2);
        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            0
        );
    }

    #[test]
    fn reader_drop_cleans_up_on_poisoned_mutex() {
        // If another thread panics while holding readers.lock(), the mutex
        // becomes poisoned. The previous `if let Ok()` would skip cleanup,
        // leaving a stale Weak in the list. unwrap_or_else recovers the poison
        // and performs the cleanup correctly.
        let rb = Arc::new(RingBuffer::new(16));
        let r = Reader::new("r".into(), rb.clone());

        // Deliberately poison the mutex from another thread.
        let rb2 = rb.clone();
        let poison_thread = std::thread::spawn(move || {
            let _guard = rb2.readers.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = poison_thread.join(); // returns Err (panicked), mutex is now poisoned

        // Verify mutex is poisoned.
        assert!(rb.readers.lock().is_err());

        // Drop should NOT silently skip: it must clean up via unwrap_or_else.
        drop(r);

        // After drop, the list is empty even though the mutex was poisoned.
        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            0
        );
    }

    #[test]
    fn reader_drop_also_prunes_other_stale_weaks() {
        // Simulate a stale Weak that was left behind (e.g. from a previous bug)
        // by manually inserting one, then verifying drop cleans it.
        let rb = Arc::new(RingBuffer::new(16));
        {
            // Insert a Weak that immediately becomes stale.
            let ephemeral = Arc::new(ReaderInfo::new("stale".into(), 0));
            rb.readers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&ephemeral));
            // ephemeral drops here → Weak becomes stale
        }
        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1
        ); // stale entry present

        let r = Reader::new("live".into(), rb.clone());
        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            2
        ); // stale + live

        drop(r);
        // drop() removes our entry AND prunes the stale one.
        assert_eq!(
            rb.readers.lock().unwrap_or_else(|e| e.into_inner()).len(),
            0
        );
    }

    #[test]
    fn test_min_read_idx_reporting() {
        let rb = Arc::new(RingBuffer::new(16));
        assert_eq!(rb.min_read_idx(), 0);

        rb.push(video_packet(0, 0, true));
        let r1 = Reader::new("r1".into(), rb.clone());
        let r2 = Reader::new("r2".into(), rb.clone());

        // Both readers start at last keyframe (0)
        assert_eq!(rb.min_read_idx(), 0);

        // Advance r1 by pulling packet
        let mut r1 = r1;
        let mut r2 = r2;
        let _ = r1.pull().unwrap();
        assert_eq!(rb.min_read_idx(), 0); // min remains 0 since r2 is at 0

        let _ = r2.pull().unwrap();
        assert_eq!(rb.min_read_idx(), 1); // both are now at 1
    }

    #[test]
    fn test_concurrent_writer_reader_no_corruption() {
        let rb = Arc::new(RingBuffer::new(4));
        rb.push(video_packet(0, 0, true));
        let mut reader = Reader::new("r1".into(), rb.clone());

        let rb_c = rb.clone();
        let writer_handle = std::thread::spawn(move || {
            for i in 1..1000 {
                rb_c.push(video_packet(i * 10, i * 10, i % 10 == 0));
                std::thread::yield_now();
            }
        });

        for _ in 0..2000 {
            match reader.pull() {
                Ok(Some(p)) => {
                    assert!(p.pts >= 0);
                }
                Ok(None) => {
                    std::thread::yield_now();
                }
                Err(e) => {
                    assert!(e.contains("Overflow"));
                }
            }
        }
        let _ = writer_handle.join();
    }

    #[test]
    fn fast_forward_with_no_keyframe_returns_live_edge() {
        // Bug #8: when no keyframe has been pushed yet (sentinel = usize::MAX)
        // fast_forward must return current_write_idx, NOT 0 or write_idx.saturating_sub(100).
        // Returning 0 when write_idx < 100 caused late-joining readers to re-scan
        // from the beginning of the ring rather than starting at the live edge.
        let rb = Arc::new(RingBuffer::new(4096));

        // Push 5 non-keyframe audio packets (no video keyframe → sentinel stays)
        for i in 0..5 {
            rb.push(MediaPacket {
                media_type: MediaType::Audio,
                track_index: 0,
                pts: i * 10,
                dts: i * 10,
                is_keyframe: false,
                format: PayloadFormat::Raw,
                payload: bytes::Bytes::from_static(b"\xAA"),
            });
        }
        // write_idx is now 5; no keyframe has been seen → sentinel usize::MAX
        let write_idx = rb.write_idx.val.load(Ordering::Relaxed);
        assert_eq!(write_idx, 5);

        // fast_forward should return write_idx (live edge), not 0
        let ff = rb.fast_forward(write_idx);
        assert_eq!(
            ff, write_idx,
            "fast_forward with no keyframe must return the live edge, not 0"
        );
    }

    #[test]
    fn fast_forward_with_keyframe_at_slot_zero() {
        // When the very first packet pushed is a video keyframe (idx=0),
        // fast_forward must still be able to find that keyframe (not confuse it
        // with the "no keyframe" sentinel).
        let rb = Arc::new(RingBuffer::new(4096));

        // Push one keyframe at slot 0
        rb.push(video_packet(0, 0, true));
        let write_idx = rb.write_idx.val.load(Ordering::Relaxed);
        assert_eq!(write_idx, 1);

        // fast_forward should return 0 (the keyframe slot), not 1 (live edge)
        let ff = rb.fast_forward(write_idx);
        assert_eq!(ff, 0, "fast_forward should return the keyframe at slot 0");
    }

    // ── Vec pre-allocation correctness ───────────────────────────────

    #[test]
    fn vec_with_capacity_retains_capacity_after_clear() {
        let cap = 65536;
        let mut v: Vec<u8> = Vec::with_capacity(cap);
        assert!(v.capacity() >= cap);
        v.extend_from_slice(&[0x47u8; 1000]);
        assert!(!v.is_empty());
        assert!(v.capacity() >= cap);
        v.clear();
        assert!(v.is_empty());
        assert!(v.capacity() >= cap);
    }

    #[test]
    fn vec_with_capacity_retains_capacity_after_drain() {
        let cap = 32;
        let mut v: Vec<(usize, bool)> = Vec::with_capacity(cap);
        for i in 0..10 {
            v.push((i, i == 0));
        }
        let cap_before = v.capacity();
        let drained_len = v.drain(..).count();
        assert_eq!(drained_len, 10);
        assert!(v.is_empty());
        assert_eq!(v.capacity(), cap_before);
    }

    #[test]
    fn vec_new_has_zero_capacity() {
        let v: Vec<u8> = Vec::new();
        assert_eq!(v.capacity(), 0);
    }

    #[test]
    fn media_packet_layout_hot_fields_in_first_cache_line() {
        // MediaPacket is 56 bytes (Bytes = 32 bytes in bytes-1.12, plus 24 bytes of other fields).
        // ArcInner<MediaPacket> = strong(8) + weak(8) + MediaPacket(56) = 72 bytes.
        //
        // #[repr(C)] with this field order ensures all hot consumer fields (media_type,
        // format, is_keyframe, track_index, pts, dts, payload.ptr, payload.len) land in
        // cache line 0 of the ArcInner (bytes 0–63), so the codec dispatch path never
        // needs a second cache line load.  See the struct-level doc for the full layout.
        assert_eq!(
            std::mem::size_of::<MediaPacket>(),
            56,
            "MediaPacket must be 56 bytes; if this fails, Bytes changed its internal layout"
        );
        // Verify field ordering: media_type must be at offset 0, payload last.
        let p = MediaPacket {
            media_type: MediaType::Video,
            format: PayloadFormat::Raw,
            is_keyframe: false,
            track_index: 0xDEAD_BEEF,
            pts: 0,
            dts: 0,
            payload: Bytes::new(),
        };
        let base = &p as *const MediaPacket as usize;
        let mt_off = &p.media_type as *const MediaType as usize - base;
        let pl_off = &p.payload as *const Bytes as usize - base;
        assert_eq!(mt_off, 0, "media_type must be the first field (offset 0)");
        assert!(
            pl_off >= 24,
            "payload must be after timestamps (offset >= 24)"
        );
        // The two enums must be exactly 1 byte each.
        assert_eq!(std::mem::size_of::<MediaType>(), 1);
        assert_eq!(std::mem::size_of::<PayloadFormat>(), 1);
    }

    // ── Fault injection ─────────────────────────────────────────────

    #[test]
    fn fault_injection_empty_payload_does_not_panic() {
        let rb = Arc::new(RingBuffer::new(16));
        rb.push(MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: 0,
            dts: 0,
            is_keyframe: true,
            format: PayloadFormat::Raw,
            payload: Bytes::new(),
        });
        let mut reader = Reader::new("empty".to_string(), rb);
        let pkt = reader.pull().unwrap().unwrap();
        assert!(pkt.payload.is_empty());
    }

    #[test]
    fn fault_injection_reordered_dts_does_not_corrupt_ring() {
        let rb = Arc::new(RingBuffer::new(16));
        rb.push(video_packet(100, 100, true));
        rb.push(video_packet(50, 50, false)); // backwards DTS
        rb.push(video_packet(200, 200, false));

        let mut reader = Reader::new("reorder".to_string(), rb);
        let p1 = reader.pull().unwrap().unwrap();
        assert_eq!(p1.dts, 100);
        let p2 = reader.pull().unwrap().unwrap();
        assert_eq!(p2.dts, 50);
        let p3 = reader.pull().unwrap().unwrap();
        assert_eq!(p3.dts, 200);
    }

    #[test]
    fn fault_injection_large_timestamp_gap_handled() {
        let rb = Arc::new(RingBuffer::new(16));
        rb.push(video_packet(0, 0, true));
        rb.push(video_packet(i64::MAX - 1, i64::MAX - 1, false));
        rb.push(video_packet(i64::MIN, i64::MIN, false));

        let mut reader = Reader::new("gap".to_string(), rb);
        assert!(reader.pull().unwrap().is_some());
        assert!(reader.pull().unwrap().is_some());
        assert!(reader.pull().unwrap().is_some());
        assert!(reader.pull().unwrap().is_none());
    }

    #[test]
    fn fault_injection_negative_timestamps_handled() {
        let rb = Arc::new(RingBuffer::new(16));
        rb.push(video_packet(-100, -100, true));
        rb.push(audio_packet(-50, -50));
        rb.push(video_packet(0, 0, false));

        let mut reader = Reader::new("negative".to_string(), rb);
        let p1 = reader.pull().unwrap().unwrap();
        assert_eq!(p1.pts, -100);
        let p2 = reader.pull().unwrap().unwrap();
        assert_eq!(p2.pts, -50);
        let p3 = reader.pull().unwrap().unwrap();
        assert_eq!(p3.pts, 0);
    }

    #[test]
    fn fault_injection_rapid_overflow_recovery() {
        let rb = Arc::new(RingBuffer::new(4));
        rb.push(video_packet(0, 0, true));
        let mut reader = Reader::new("overflow_recovery".to_string(), rb.clone());

        for i in 1..20 {
            rb.push(video_packet(i * 33, i * 33, i == 10));
        }

        let err = reader.pull().unwrap_err();
        assert!(err.contains("Overflow"));

        rb.push(video_packet(1000, 1000, true));
        rb.push(video_packet(1033, 1033, false));

        let pkt = reader.pull().unwrap().unwrap();
        assert!(pkt.pts >= 1000);
    }

    #[test]
    fn fault_injection_mixed_format_payloads() {
        let rb = Arc::new(RingBuffer::new(16));
        rb.push(MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: 0,
            dts: 0,
            is_keyframe: true,
            format: PayloadFormat::Flv,
            payload: Bytes::from_static(&[0x17, 0x01, 0, 0, 0]),
        });
        rb.push(MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: 33,
            dts: 33,
            is_keyframe: false,
            format: PayloadFormat::Raw,
            payload: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        });

        let mut reader = Reader::new("mixed_fmt".to_string(), rb);
        let p1 = reader.pull().unwrap().unwrap();
        assert_eq!(p1.format, PayloadFormat::Flv);
        let p2 = reader.pull().unwrap().unwrap();
        assert_eq!(p2.format, PayloadFormat::Raw);
    }

    #[test]
    fn fault_injection_high_track_index() {
        let rb = Arc::new(RingBuffer::new(16));
        rb.push(MediaPacket {
            media_type: MediaType::Video,
            track_index: u32::MAX,
            pts: 0,
            dts: 0,
            is_keyframe: true,
            format: PayloadFormat::Raw,
            payload: Bytes::from_static(&[0; 4]),
        });
        let mut reader = Reader::new("high_track".to_string(), rb);
        let pkt = reader.pull().unwrap().unwrap();
        assert_eq!(pkt.track_index, u32::MAX);
    }

    #[test]
    fn fault_injection_push_batch_with_no_keyframes() {
        let rb = Arc::new(RingBuffer::new(16));
        // Create reader before push so it starts at write_idx=0 and sees all packets.
        let mut reader = Reader::new_live("no_kf".to_string(), rb.clone());
        let count = rb.push_batch([
            video_packet(10, 10, false),
            video_packet(20, 20, false),
            video_packet(30, 30, false),
        ]);
        assert_eq!(count, 3);
        let mut out = Vec::new();
        let n = reader.pull_burst(&mut out, 32).unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn dts_enforcer_fault_injection_extreme_backwards_jump() {
        let mut e = DtsEnforcer::new(1);
        assert_eq!(e.enforce(0, 1_000_000, 1_000_000), (1_000_000, 1_000_000));
        let (pts, dts) = e.enforce(0, 0, 0);
        assert!(dts > 1_000_000, "DTS must be bumped past previous value");
        assert!(pts >= dts, "PTS must be >= DTS");
    }

    #[test]
    fn dts_enforcer_fault_injection_negative_pts_dts() {
        let mut e = DtsEnforcer::new(1);
        let (pts, dts) = e.enforce(0, -100, -100);
        assert_eq!((pts, dts), (-100, -100));
        let (pts2, dts2) = e.enforce(0, -200, -200);
        assert!(dts2 > -100, "DTS must be bumped past -100");
        assert!(pts2 >= dts2);
    }

    #[test]
    fn vec_with_capacity_reuses_allocation_across_cycles() {
        let cap = 65536;
        let mut v: Vec<u8> = Vec::with_capacity(cap);
        let alloc_id = v.as_ptr() as usize;
        for _ in 0..3 {
            v.extend_from_slice(&[0x47u8; 1000]);
            v.clear();
            assert_eq!(v.as_ptr() as usize, alloc_id);
        }
    }

    #[test]
    fn pull_burst_records_burst_stats() {
        let rb = Arc::new(RingBuffer::new(64));
        // Push 5 packets so first burst yields 5, then push 1 for a size-1 burst.
        for i in 0i64..5 {
            rb.push(video_packet(i * 33, i * 33, i == 0));
        }
        let mut reader = Reader::new("stats_test".to_string(), rb.clone());
        let mut out = Vec::new();

        // Burst of 5
        let n = reader.pull_burst(&mut out, 32).unwrap();
        assert_eq!(n, 5);

        // Push 1 more; burst of 1
        rb.push(video_packet(5_i64 * 33, 5_i64 * 33, false));
        let n2 = reader.pull_burst(&mut out, 32).unwrap();
        assert_eq!(n2, 1);

        let (avg, median, bursts) = reader.info.burst_stats();
        assert_eq!(bursts, 2, "two non-empty burst calls");
        // avg = (5+1)/2 = 3.0
        assert!((avg - 3.0).abs() < 0.01, "avg burst = {avg}");
        // n=5 lands in the 5-8 bucket and n=1 lands in the size-1 bucket.
        // The approximate median walks buckets by count, so the size-1 bucket wins.
        assert_eq!(median, 1, "median burst = {median}");

        // Empty pull does not record a burst
        let n3 = reader.pull_burst(&mut out, 32).unwrap();
        assert_eq!(n3, 0);
        let (_, _, bursts2) = reader.info.burst_stats();
        assert_eq!(bursts2, 2, "empty pull must not increment burst_count");
    }

    #[test]
    fn payload_stats_reports_retained_ring_bytes() {
        let rb = Arc::new(RingBuffer::new(3));
        rb.push(MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: 0,
            dts: 0,
            is_keyframe: true,
            format: PayloadFormat::Raw,
            payload: Bytes::from(vec![1; 10]),
        });
        rb.push(MediaPacket {
            media_type: MediaType::Audio,
            track_index: 0,
            pts: 0,
            dts: 0,
            is_keyframe: false,
            format: PayloadFormat::Raw,
            payload: Bytes::from(vec![2; 4]),
        });

        let stats = rb.payload_stats();
        assert_eq!(stats.slots, 2);
        assert_eq!(stats.payload_bytes, 14);
        assert_eq!(stats.video_bytes, 10);
        assert_eq!(stats.audio_bytes, 4);
        assert_eq!(stats.min_payload_bytes, 4);
        assert_eq!(stats.max_payload_bytes, 10);
    }
}
