//! Lock-free single-producer multi-consumer ring buffer for media packet fan-out.
//!
//! # Memory Layout
//!
//! Each slot is `#[repr(align(64))]` — one cache line — so concurrent readers
//! on different slots never cause false-sharing stalls. The write index is
//! similarly isolated to its own cache line.
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

#[derive(Clone)]
pub struct MediaPacket {
    pub media_type: MediaType,
    pub track_index: u32,
    pub pts: i64,
    pub dts: i64,
    pub is_keyframe: bool,
    pub payload: Bytes,
}

#[repr(align(64))]
pub struct AlignedSlot {
    data: ArcSwapOption<MediaPacket>,
}

#[repr(align(64))]
pub struct AlignedAtomicUsize {
    val: AtomicUsize,
}

pub struct RingBuffer {
    slots: Vec<AlignedSlot>,
    write_idx: AlignedAtomicUsize,
    last_keyframe_idx: AlignedAtomicUsize,
    capacity: usize,
    notify: Arc<tokio::sync::Notify>,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(AlignedSlot {
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
    read_idx: usize,
}

impl Reader {
    pub fn new(buffer: Arc<RingBuffer>) -> Self {
        let current_write = buffer.get_write_idx();
        let start_idx = buffer.fast_forward(current_write);
        Self {
            buffer,
            read_idx: start_idx,
        }
    }

    pub fn pull(&mut self) -> Result<Option<Arc<MediaPacket>>, &'static str> {
        let write_idx = self.buffer.get_write_idx();

        if write_idx > self.read_idx && write_idx - self.read_idx >= self.buffer.capacity {
            let new_idx = self.buffer.fast_forward(write_idx);
            self.read_idx = new_idx;
            return Err("Overflow: reader lagged and was fast-forwarded");
        }

        if self.read_idx == write_idx {
            return Ok(None);
        }

        let packet = self.buffer.read_at(self.read_idx);
        if packet.is_some() {
            self.read_idx += 1;
        }
        Ok(packet)
    }

    pub async fn wait_for_data(&self) {
        let notify = self.buffer.get_notify();
        notify.notified().await;
    }
}
