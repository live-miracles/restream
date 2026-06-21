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
use std::sync::{Condvar, Mutex};

const AVIO_BUFFER_SIZE: usize = 32768;

pub struct MemoryQueue {
    inner: Mutex<MemoryQueueInner>,
    cvar: Condvar,
}

struct MemoryQueueInner {
    buf: VecDeque<u8>,
    closed: bool,
}

impl Default for MemoryQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MemoryQueueInner {
                buf: VecDeque::new(),
                closed: false,
            }),
            cvar: Condvar::new(),
        }
    }

    pub fn write(&self, data: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        inner.buf.extend(data.iter().copied());
        self.cvar.notify_all();
    }

    /// Append multiple chunks while taking the queue lock and notifying once.
    pub fn write_batch<'a, I>(&self, chunks: I) -> usize
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let mut inner = self.inner.lock().unwrap();
        let mut bytes = 0usize;
        for chunk in chunks {
            bytes += chunk.len();
            inner.buf.extend(chunk.iter().copied());
        }
        if bytes > 0 {
            self.cvar.notify_all();
        }
        bytes
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        self.cvar.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    pub fn read(&self, target: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
        while inner.buf.is_empty() && !inner.closed {
            inner = self.cvar.wait(inner).unwrap();
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
        to_read
    }

    pub fn read_nonblocking(&self, target: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
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
        to_read
    }
}

pub struct CustomInput {
    pub input: Option<ffmpeg::format::context::Input>,
    avio_ctx: *mut ffmpeg::ffi::AVIOContext,
}

impl CustomInput {
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
                Some(write_packet_cb),
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
        unsafe {
            if let Some(mut output) = self.output.take() {
                // `ffmpeg-next`'s output destructor unconditionally calls
                // avio_close(pb). Detach our custom AVIOContext so it is not
                // closed there and then freed a second time below.
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

unsafe extern "C" fn write_packet_cb(opaque: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let queue = unsafe { &*(opaque as *const MemoryQueue) };
    let slice = unsafe { std::slice::from_raw_parts(buf, buf_size as usize) };
    queue.write(slice);
    buf_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_batch_preserves_chunk_order() {
        let queue = MemoryQueue::new();
        assert_eq!(queue.write_batch([b"abc".as_slice(), b"def".as_slice()]), 6);

        let mut output = [0u8; 6];
        assert_eq!(queue.read(&mut output), output.len());
        assert_eq!(&output, b"abcdef");
    }

    #[test]
    fn empty_write_batch_does_not_add_data() {
        let queue = MemoryQueue::new();
        assert_eq!(queue.write_batch(std::iter::empty()), 0);
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
}
