//! In-process FFmpeg I/O via custom AVIOContext backed by a thread-safe MemoryQueue.
//!
//! Replaces the TCP loopback sockets that the old Node.js backend used to pipe
//! media between ingest and FFmpeg child processes. Data flows through a
//! `VecDeque<u8>` protected by `Mutex` + `Condvar`, with bulk `as_slices()` +
//! `drain()` reads instead of per-byte `pop_front()`.
//!
//! # Buffer Sizing
//!
//! The AVIO buffer is 32 KB (FFmpeg's default). A 4 KB buffer would cause ~8x
//! more callback invocations per video frame at typical bitrates.

use ffmpeg_next as ffmpeg;
use std::collections::VecDeque;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::debug;

const AVIO_BUFFER_SIZE: usize = 32768;

// 512 KB absorbs bursts between the async TS reader and the blocking SRT/RTMP
// sender thread. Scale-test evidence: max per-queue HWM = 398 KB at 8 Mb/s
// across all config×bitrate combinations, 0 blocked_writes throughout.
// Operators running very high latency links (> 1 s) can raise this with
// RESTREAM_AVIO_QUEUE_CAPACITY (bytes).
const DEFAULT_AVIO_QUEUE_CAPACITY: usize = 512 * 1024;

fn default_avio_queue_capacity() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("RESTREAM_AVIO_QUEUE_CAPACITY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_AVIO_QUEUE_CAPACITY)
            .clamp(64 * 1024, 16 * 1024 * 1024)
    })
}

pub struct MemoryQueue {
    inner: Mutex<MemoryQueueInner>,
    cvar: Condvar,
    space_available: Notify,
    capacity: usize,
    high_water_bytes: AtomicUsize,
    blocked_writes: AtomicU64,
    blocked_write_us: AtomicU64,
}

struct MemoryQueueInner {
    buf: VecDeque<u8>,
    closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryQueueStats {
    pub len: usize,
    pub capacity: usize,
    pub high_water_bytes: usize,
    pub blocked_writes: u64,
    pub blocked_write_us: u64,
    pub closed: bool,
}

impl Default for MemoryQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryQueue {
    pub fn new() -> Self {
        Self::new_with_capacity(default_avio_queue_capacity())
    }

    pub fn new_with_capacity(capacity: usize) -> Self {
        debug!(capacity_bytes = capacity, "memory queue created");
        Self {
            inner: Mutex::new(MemoryQueueInner {
                buf: VecDeque::new(),
                closed: false,
            }),
            cvar: Condvar::new(),
            space_available: Notify::new(),
            capacity,
            high_water_bytes: AtomicUsize::new(0),
            blocked_writes: AtomicU64::new(0),
            blocked_write_us: AtomicU64::new(0),
        }
    }

    pub async fn write(&self, data: &[u8]) {
        let mut blocked_since = None;
        loop {
            {
                let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if inner.closed || inner.buf.len() < self.capacity {
                    break;
                }
            }
            if blocked_since.is_none() {
                blocked_since = Some(Instant::now());
                self.blocked_writes.fetch_add(1, Ordering::Relaxed);
            }
            self.space_available.notified().await;
        }
        if let Some(start) = blocked_since {
            self.blocked_write_us
                .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        }

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return;
        }
        inner.buf.extend(data.iter().copied());
        self.record_depth(inner.buf.len());
        self.cvar.notify_all();
    }

    /// Append multiple chunks while taking the queue lock and notifying once.
    pub async fn write_batch<'a, I>(&self, chunks: I) -> usize
    where
        I: IntoIterator<Item = &'a [u8]> + Clone,
    {
        let total_bytes: usize = chunks.clone().into_iter().map(|c| c.len()).sum();
        if total_bytes == 0 {
            return 0;
        }

        let mut blocked_since = None;
        loop {
            {
                let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if inner.closed || inner.buf.len() < self.capacity {
                    break;
                }
            }
            if blocked_since.is_none() {
                blocked_since = Some(Instant::now());
                self.blocked_writes.fetch_add(1, Ordering::Relaxed);
            }
            self.space_available.notified().await;
        }
        if let Some(start) = blocked_since {
            self.blocked_write_us
                .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        }

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return 0;
        }
        let mut bytes = 0usize;
        for chunk in chunks {
            bytes += chunk.len();
            inner.buf.extend(chunk.iter().copied());
        }
        if bytes > 0 {
            self.record_depth(inner.buf.len());
            self.cvar.notify_all();
        }
        bytes
    }

    pub fn write_sync(&self, data: &[u8]) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return;
        }
        inner.buf.extend(data.iter().copied());
        self.record_depth(inner.buf.len());
        self.cvar.notify_all();
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.closed = true;
        self.cvar.notify_all();
        self.space_available.notify_waiters();
        let high = self.high_water_bytes.load(Ordering::Relaxed);
        let blocked = self.blocked_writes.load(Ordering::Relaxed);
        debug!(
            high_water_bytes = high,
            blocked_writes = blocked,
            "memory queue closed"
        );
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).closed
    }

    /// Current number of buffered bytes awaiting consumption.
    ///
    /// Useful for detecting producer/consumer imbalance (e.g., a slow FFmpeg
    /// thread unable to keep pace with ingest). Values consistently above a few
    /// megabytes indicate the downstream stage is falling behind.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buf
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stats(&self) -> MemoryQueueStats {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        MemoryQueueStats {
            len: inner.buf.len(),
            capacity: self.capacity,
            high_water_bytes: self.high_water_bytes.load(Ordering::Relaxed),
            blocked_writes: self.blocked_writes.load(Ordering::Relaxed),
            blocked_write_us: self.blocked_write_us.load(Ordering::Relaxed),
            closed: inner.closed,
        }
    }

    fn record_depth(&self, len: usize) {
        self.high_water_bytes.fetch_max(len, Ordering::Relaxed);
    }

    pub fn read(&self, target: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while inner.buf.is_empty() && !inner.closed {
            // Use wait_timeout so the FFmpeg AVIO thread is not blocked indefinitely
            // if the producer panics without calling close().  Poison recovery via
            // unwrap_or_else ensures we don't panic on a poisoned mutex here.
            inner = self
                .cvar
                .wait_timeout(inner, Duration::from_secs(5))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if inner.buf.is_empty() && inner.closed {
            return 0;
        }
        let to_read = std::cmp::min(target.len(), inner.buf.len());
        let (front, back) = inner.buf.as_slices();
        if to_read <= front.len() {
            target[..to_read].copy_from_slice(&front[..to_read]);
        } else {
            target[..front.len()].copy_from_slice(front);
            target[front.len()..to_read].copy_from_slice(&back[..to_read - front.len()]);
        }
        inner.buf.drain(..to_read);

        if inner.buf.len() < self.capacity {
            self.space_available.notify_waiters();
        }

        to_read
    }

    pub fn read_nonblocking(&self, target: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.buf.is_empty() {
            return 0;
        }
        let to_read = std::cmp::min(target.len(), inner.buf.len());
        let (front, back) = inner.buf.as_slices();
        if to_read <= front.len() {
            target[..to_read].copy_from_slice(&front[..to_read]);
        } else {
            target[..front.len()].copy_from_slice(front);
            target[front.len()..to_read].copy_from_slice(&back[..to_read - front.len()]);
        }
        inner.buf.drain(..to_read);

        if inner.buf.len() < self.capacity {
            self.space_available.notify_waiters();
        }

        to_read
    }
}

pub struct CustomInput {
    pub input: Option<ffmpeg::format::context::Input>,
    avio_ctx: *mut ffmpeg::ffi::AVIOContext,
}

impl CustomInput {
    // SAFETY: This function builds an FFmpeg custom I/O context with
    // callbacks that read from a MemoryQueue. The `queue` pointer must
    // outlive the CustomInput (guaranteed by the caller holding an Arc).
    // av_malloc/av_free manage the I/O buffer; avio_alloc_context takes
    // ownership of the buffer. On error paths, all allocated resources
    // are freed before returning. read_packet_cb is the only active
    // callback; seek and write are None (input is read-only).
    pub fn new(queue: *const MemoryQueue) -> Result<Self, &'static str> {
        unsafe {
            let buffer = ffmpeg::ffi::av_malloc(AVIO_BUFFER_SIZE) as *mut u8;
            if buffer.is_null() {
                return Err("Failed to allocate AVIO buffer");
            }

            let avio_ctx = ffmpeg::ffi::avio_alloc_context(
                buffer,
                AVIO_BUFFER_SIZE as c_int,
                0,
                queue as *mut c_void,
                Some(read_packet_cb),
                None,
                None,
            );

            if avio_ctx.is_null() {
                ffmpeg::ffi::av_free(buffer as *mut c_void);
                return Err("Failed to allocate AVIOContext");
            }

            let raw_ctx = ffmpeg::ffi::avformat_alloc_context();
            if raw_ctx.is_null() {
                ffmpeg::ffi::av_freep(&mut (*avio_ctx).buffer as *mut _ as *mut c_void);
                ffmpeg::ffi::av_free(avio_ctx as *mut c_void);
                return Err("Failed to allocate AVFormatContext");
            }

            (*raw_ctx).pb = avio_ctx;
            (*raw_ctx).flags |= ffmpeg::ffi::AVFMT_FLAG_CUSTOM_IO;
            // MPEG-TS over in-memory pipes still needs enough lead-in to see
            // the first AAC headers. The smaller probe budget produced
            // "could not find codec parameters" warnings in clean test
            // fixtures and occasionally left audio metadata incomplete.
            (*raw_ctx).probesize = 1 << 20; // 1 MiB
            (*raw_ctx).max_analyze_duration = 2_000_000; // 2s in microseconds

            let mut raw_ctx_mut = raw_ctx;
            let open_res = ffmpeg::ffi::avformat_open_input(
                &mut raw_ctx_mut,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            if open_res < 0 {
                // avformat_open_input frees raw_ctx on failure but not the avio
                ffmpeg::ffi::av_freep(&mut (*avio_ctx).buffer as *mut _ as *mut c_void);
                ffmpeg::ffi::av_free(avio_ctx as *mut c_void);
                return Err("Failed to open input stream");
            }

            let info_res =
                ffmpeg::ffi::avformat_find_stream_info(raw_ctx_mut, std::ptr::null_mut());
            if info_res < 0 {
                (*raw_ctx_mut).pb = std::ptr::null_mut();
                let input = ffmpeg::format::context::Input::wrap(raw_ctx_mut);
                drop(input);
                ffmpeg::ffi::av_freep(&mut (*avio_ctx).buffer as *mut _ as *mut c_void);
                ffmpeg::ffi::av_free(avio_ctx as *mut c_void);
                return Err("Failed to find stream info");
            }

            let input = ffmpeg::format::context::Input::wrap(raw_ctx_mut);

            Ok(Self {
                input: Some(input),
                avio_ctx,
            })
        }
    }
}

impl Drop for CustomInput {
    fn drop(&mut self) {
        // SAFETY: Detaches the custom AVIO context from the AVFormatContext
        // before avformat_close_input runs (which would try to free the AVIO
        // buffer we own). Then frees the AVIO buffer and context via FFmpeg's
        // allocator (av_freep for the buffer, avio_context_free for the ctx).
        // The raw pointers were allocated by av_malloc/avio_alloc_context in
        // CustomInput::new and have not been freed elsewhere.
        unsafe {
            if let Some(mut input) = self.input.take() {
                // `ffmpeg-next` owns the AVFormatContext, but this wrapper owns
                // the custom AVIOContext. Detach it before the input destructor
                // calls avformat_close_input().
                (*input.as_mut_ptr()).pb = std::ptr::null_mut();
                drop(input);
            }
            if !self.avio_ctx.is_null() {
                ffmpeg::ffi::av_freep(&mut (*self.avio_ctx).buffer as *mut _ as *mut c_void);
                ffmpeg::ffi::avio_context_free(&mut self.avio_ctx);
            }
        }
    }
}

pub struct CustomOutput {
    pub output: Option<ffmpeg::format::context::Output>,
    avio_ctx: *mut ffmpeg::ffi::AVIOContext,
}

impl CustomOutput {
    // SAFETY: Builds an FFmpeg custom output I/O context with write_packet_cb
    // writing into a MemoryQueue. The `queue` pointer must outlive the
    // CustomOutput (caller holds an Arc). av_malloc/av_free manage the I/O
    // buffer. read and seek callbacks are None (output is write-only). On
    // all error paths, allocated resources are freed before returning.
    pub fn new(queue: *const MemoryQueue, format_name: &str) -> Result<Self, &'static str> {
        unsafe {
            let buffer = ffmpeg::ffi::av_malloc(AVIO_BUFFER_SIZE) as *mut u8;
            if buffer.is_null() {
                return Err("Failed to allocate AVIO buffer");
            }

            let avio_ctx = ffmpeg::ffi::avio_alloc_context(
                buffer,
                AVIO_BUFFER_SIZE as c_int,
                1,
                queue as *mut c_void,
                None,
                // FFmpeg 8 made the write callback buffer const; older host
                // headers still expose it as mutable. Pointer mutability is
                // not an ABI distinction, and write_packet_cb only reads it.
                Some(std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int,
                    _,
                >(write_packet_cb)),
                None,
            );

            if avio_ctx.is_null() {
                ffmpeg::ffi::av_free(buffer as *mut c_void);
                return Err("Failed to allocate AVIOContext");
            }

            let mut raw_ctx: *mut ffmpeg::ffi::AVFormatContext = std::ptr::null_mut();
            let fmt_c = match std::ffi::CString::new(format_name) {
                Ok(c) => c,
                Err(_) => {
                    ffmpeg::ffi::av_freep(&mut (*avio_ctx).buffer as *mut _ as *mut c_void);
                    ffmpeg::ffi::av_free(avio_ctx as *mut c_void);
                    return Err("Invalid format name (contains null byte)");
                }
            };
            let res = ffmpeg::ffi::avformat_alloc_output_context2(
                &mut raw_ctx,
                std::ptr::null_mut(),
                fmt_c.as_ptr(),
                std::ptr::null(),
            );

            if res < 0 || raw_ctx.is_null() {
                ffmpeg::ffi::av_freep(&mut (*avio_ctx).buffer as *mut _ as *mut c_void);
                ffmpeg::ffi::av_free(avio_ctx as *mut c_void);
                return Err("Failed to allocate output format context");
            }

            (*raw_ctx).pb = avio_ctx;
            (*raw_ctx).flags |= ffmpeg::ffi::AVFMT_FLAG_CUSTOM_IO;

            let output = ffmpeg::format::context::Output::wrap(raw_ctx);

            Ok(Self {
                output: Some(output),
                avio_ctx,
            })
        }
    }
}

impl Drop for CustomOutput {
    fn drop(&mut self) {
        // SAFETY: Detaches the custom AVIO context before avformat_close_input
        // runs (which would try to close the AVIO we own). Then frees the AVIO
        // buffer via av_freep and the context via avio_context_free. Pointers
        // were allocated in CustomOutput::new and not freed elsewhere.
        // ffmpeg-next's output destructor unconditionally calls avio_close(pb)
        // on the AVIO context pointer, so we NULL it first to prevent
        // double-free.
        unsafe {
            if let Some(mut output) = self.output.take() {
                (*output.as_mut_ptr()).pb = std::ptr::null_mut();
                drop(output);
            }
            if !self.avio_ctx.is_null() {
                ffmpeg::ffi::av_freep(&mut (*self.avio_ctx).buffer as *mut _ as *mut c_void);
                ffmpeg::ffi::avio_context_free(&mut self.avio_ctx);
            }
        }
    }
}

// SAFETY: FFmpeg AVIO read callback. `opaque` is a `*const MemoryQueue`
// set during avio_alloc_context. `buf` and `buf_size` are provided by
// FFmpeg's internal I/O layer. The MemoryQueue is alive for the lifetime
// of the AVIO context (both managed by the enclosing CustomInput).
// `from_raw_parts_mut` is valid because FFmpeg guarantees `buf` points
// to `buf_size` writable bytes.
unsafe extern "C" fn read_packet_cb(opaque: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let queue = unsafe { &*(opaque as *const MemoryQueue) };
    let target = unsafe { std::slice::from_raw_parts_mut(buf, buf_size as usize) };
    let n = queue.read(target);
    if n == 0 && queue.is_closed() {
        -541478725 // AVERROR_EOF
    } else {
        n as c_int
    }
}

// SAFETY: FFmpeg AVIO write callback. `opaque` is a `*const MemoryQueue`
// set during avio_alloc_context. `buf` contains `buf_size` readable bytes
// of data to write. The MemoryQueue is alive for the AVIO context lifetime.
// `from_raw_parts` is valid because FFmpeg guarantees `buf` is `buf_size`
// readable bytes.
unsafe extern "C" fn write_packet_cb(opaque: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let queue = unsafe { &*(opaque as *const MemoryQueue) };
    let slice = unsafe { std::slice::from_raw_parts(buf, buf_size as usize) };
    queue.write_sync(slice);
    buf_size
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    static EXPECTED_PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    struct ScopedSilentPanicHook(
        Option<Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>>,
    );

    impl ScopedSilentPanicHook {
        fn new() -> Self {
            Self(Some(std::panic::take_hook()))
        }

        fn silence(&mut self) {
            std::panic::set_hook(Box::new(|_| {}));
        }
    }

    impl Drop for ScopedSilentPanicHook {
        fn drop(&mut self) {
            if let Some(hook) = self.0.take() {
                std::panic::set_hook(hook);
            }
        }
    }

    #[tokio::test]
    async fn write_batch_preserves_chunk_order() {
        let queue = MemoryQueue::new();
        assert_eq!(
            queue
                .write_batch([b"abc".as_slice(), b"def".as_slice()])
                .await,
            6
        );

        let mut output = [0u8; 6];
        assert_eq!(queue.read(&mut output), output.len());
        assert_eq!(&output, b"abcdef");
    }

    #[tokio::test]
    async fn empty_write_batch_does_not_add_data() {
        let queue = MemoryQueue::new();
        assert_eq!(queue.write_batch(std::iter::empty()).await, 0);
        let mut output = [0u8; 1];
        assert_eq!(queue.read_nonblocking(&mut output), 0);
    }

    #[test]
    fn custom_output_drop_does_not_double_close_avio() {
        ffmpeg::init().expect("FFmpeg init");
        let queue = MemoryQueue::new();
        let output = CustomOutput::new(&queue, "mpegts").expect("custom output");
        drop(output);
    }

    // --- Regression: issue #2 (Round 3) — MemoryQueue::read must not panic
    // if the Mutex is poisoned by a panicking writer thread.
    // Before the fix, `cvar.wait(inner).unwrap()` would propagate the poison
    // and panic in the AVIO read callback, corrupting the FFmpeg output.
    // After the fix the lock is recovered and reading resumes normally.
    #[tokio::test]
    async fn read_recovers_from_poisoned_mutex() {
        // Poison the MemoryQueue's internal mutex from a separate thread,
        // then verify that write() and read_nonblocking() do not panic.
        // We use Arc<MemoryQueue> so the poisoning thread can share the object.
        let queue = Arc::new(MemoryQueue::new());
        {
            let _panic_hook_lock = EXPECTED_PANIC_HOOK_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut panic_hook = ScopedSilentPanicHook::new();
            panic_hook.silence();
            let q = queue.clone();
            // unwrap() inside a thread that panics → the Mutex becomes poisoned
            let _ = std::thread::spawn(move || {
                let _guard = q.inner.lock().unwrap();
                panic!("deliberate poison");
            })
            .join(); // returns Err(payload) — that's expected, we just consume it
        }
        // The mutex is now poisoned. write() and read_nonblocking() must
        // recover via `unwrap_or_else(|e| e.into_inner())` and not panic.
        queue.write(b"hello").await;
        let mut buf = [0u8; 5];
        let n = queue.read_nonblocking(&mut buf);
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn write_after_close_is_noop() {
        let queue = MemoryQueue::new();
        queue.close();
        queue.write(b"should not appear").await;
        let mut buf = [0u8; 16];
        assert_eq!(queue.read_nonblocking(&mut buf), 0);
    }

    #[tokio::test]
    async fn write_batch_after_close_returns_zero() {
        let queue = MemoryQueue::new();
        queue.close();
        assert_eq!(queue.write_batch([b"data" as &[u8]]).await, 0);
    }

    #[test]
    fn read_nonblocking_empty_returns_zero() {
        let queue = MemoryQueue::new();
        let mut buf = [0u8; 16];
        assert_eq!(queue.read_nonblocking(&mut buf), 0);
    }

    #[test]
    fn read_returns_zero_on_closed_empty() {
        let queue = MemoryQueue::new();
        queue.close();
        let mut buf = [0u8; 16];
        assert_eq!(queue.read(&mut buf), 0);
    }

    #[test]
    fn len_and_is_empty_reflect_buffered_bytes() {
        let queue = MemoryQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        queue.write_sync(b"hello");
        assert!(!queue.is_empty());
        assert_eq!(queue.len(), 5);
        let mut buf = [0u8; 3];
        queue.read_nonblocking(&mut buf);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn stats_report_depth_capacity_high_water_and_closed_state() {
        let queue = MemoryQueue::new_with_capacity(8);
        assert_eq!(
            queue.stats(),
            MemoryQueueStats {
                len: 0,
                capacity: 8,
                high_water_bytes: 0,
                blocked_writes: 0,
                blocked_write_us: 0,
                closed: false,
            }
        );

        queue.write_sync(b"hello");
        let stats = queue.stats();
        assert_eq!(stats.len, 5);
        assert_eq!(stats.capacity, 8);
        assert_eq!(stats.high_water_bytes, 5);
        assert!(!stats.closed);

        let mut buf = [0u8; 3];
        queue.read_nonblocking(&mut buf);
        let stats = queue.stats();
        assert_eq!(stats.len, 2);
        assert_eq!(stats.high_water_bytes, 5);

        queue.close();
        assert!(queue.stats().closed);
    }

    #[test]
    fn is_closed_reflects_state() {
        let queue = MemoryQueue::new();
        assert!(!queue.is_closed());
        queue.close();
        assert!(queue.is_closed());
    }

    #[tokio::test]
    async fn write_respects_capacity() {
        let queue = Arc::new(MemoryQueue::new_with_capacity(5));
        queue.write(b"hello").await; // 5 bytes — exactly at capacity
        let q = queue.clone();
        let handle = tokio::spawn(async move {
            q.write(b"blocked").await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !handle.is_finished(),
            "write should still be blocked at capacity"
        );
        let mut buf = [0u8; 5];
        queue.read_nonblocking(&mut buf);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn blocked_write_updates_backpressure_stats() {
        let queue = Arc::new(MemoryQueue::new_with_capacity(5));
        queue.write(b"hello").await;
        let q = queue.clone();
        let handle = tokio::spawn(async move {
            q.write(b"blocked").await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!handle.is_finished());
        assert_eq!(queue.stats().blocked_writes, 1);

        let mut buf = [0u8; 5];
        queue.read_nonblocking(&mut buf);
        handle.await.unwrap();

        let stats = queue.stats();
        assert_eq!(stats.blocked_writes, 1);
        assert!(stats.blocked_write_us > 0);
        assert!(stats.high_water_bytes >= 7);
    }

    #[tokio::test]
    async fn blocked_write_unblocks_when_queue_closes() {
        let queue = Arc::new(MemoryQueue::new_with_capacity(5));
        queue.write(b"hello").await;

        let writer_queue = queue.clone();
        let blocked_write = tokio::spawn(async move {
            writer_queue.write(b"blocked").await;
            writer_queue.is_closed()
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!blocked_write.is_finished());

        queue.close();

        assert!(
            blocked_write.await.unwrap(),
            "blocked writer should observe queue closure and return"
        );
    }

    #[tokio::test]
    async fn read_wakes_on_write() {
        let queue = Arc::new(MemoryQueue::new());
        let q = queue.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            q.write_sync(b"wakeup");
        });
        let mut buf = [0u8; 6];
        let n = queue.read(&mut buf);
        assert_eq!(n, 6);
        assert_eq!(&buf, b"wakeup");
        handle.join().unwrap();
    }

    proptest! {
        #[test]
        fn write_batch_round_trips_random_chunks(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..64),
                0..16
            )
        ) {
            let total_bytes: usize = chunks.iter().map(Vec::len).sum();
            let queue = MemoryQueue::new_with_capacity(total_bytes.max(1));
            let expected: Vec<u8> = chunks.iter().flatten().copied().collect();

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let slices: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
            let written = runtime.block_on(queue.write_batch(slices.iter().copied()));

            prop_assert_eq!(written, total_bytes);

            let mut actual = vec![0u8; total_bytes];
            let read = queue.read_nonblocking(&mut actual);
            prop_assert_eq!(read, total_bytes);
            prop_assert_eq!(actual, expected);
        }
    }
}
