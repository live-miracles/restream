//! Thin MPEG-TS chunk ring wrapper that gives TS consumers a cancelable view
//! over pre-muxed packets stored in the shared ring buffer.

use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};
use bytes::Bytes;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A thin wrapper around Arc<RingBuffer> where packets hold pre-muxed MPEG-TS chunks.
pub struct TsChunkRing {
    pub ring: Arc<RingBuffer>,
    pub cancel: CancellationToken,
}

impl TsChunkRing {
    pub fn new(capacity: usize, cancel: CancellationToken) -> Self {
        Self {
            ring: Arc::new(RingBuffer::new(capacity)),
            cancel,
        }
    }

    pub fn push(&self, payload: Bytes, is_keyframe: bool) {
        let packet = MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: 0,
            dts: 0,
            is_keyframe,
            format: PayloadFormat::Raw,
            payload,
        };
        self.ring.push(packet);
    }

    pub fn push_batch<I>(&self, payloads: I) -> usize
    where
        I: IntoIterator<Item = (Bytes, bool)>,
    {
        let packets = payloads
            .into_iter()
            .map(|(payload, is_keyframe)| MediaPacket {
                media_type: MediaType::Video,
                track_index: 0,
                pts: 0,
                dts: 0,
                is_keyframe,
                format: PayloadFormat::Raw,
                payload,
            });
        self.ring.push_batch(packets)
    }
}

pub struct TsChunkReader {
    inner: Reader,
    cancel: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsChunkWaitResult {
    Data,
    Cancelled,
}

impl TsChunkReader {
    pub fn new(name: String, ring: &TsChunkRing) -> Self {
        Self {
            inner: Reader::new(name, ring.ring.clone()),
            cancel: ring.cancel.clone(),
        }
    }

    pub async fn wait_for_data_or_cancelled(&mut self) -> TsChunkWaitResult {
        tokio::select! {
            _ = self.cancel.cancelled() => TsChunkWaitResult::Cancelled,
            _ = self.inner.wait_for_data() => TsChunkWaitResult::Data,
        }
    }

    pub fn pull_burst(
        &mut self,
        output: &mut Vec<Arc<MediaPacket>>,
        max_packets: usize,
    ) -> Result<usize, &'static str> {
        self.inner.pull_burst(output, max_packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_readers_receive_same_chunks() {
        let cancel = CancellationToken::new();
        let ts_ring = TsChunkRing::new(16, cancel);

        let mut r1 = TsChunkReader::new("reader1".to_string(), &ts_ring);
        let mut r2 = TsChunkReader::new("reader2".to_string(), &ts_ring);

        // Push some chunks
        ts_ring.push(Bytes::from_static(b"chunk1"), true);
        ts_ring.push(Bytes::from_static(b"chunk2"), false);
        ts_ring.push_batch(vec![
            (Bytes::from_static(b"chunk3"), false),
            (Bytes::from_static(b"chunk4"), false),
        ]);

        let mut out1 = Vec::new();
        let mut out2 = Vec::new();

        let count1 = r1.pull_burst(&mut out1, 10).unwrap();
        let count2 = r2.pull_burst(&mut out2, 10).unwrap();

        assert_eq!(count1, 4);
        assert_eq!(count2, 4);

        let payloads1: Vec<&[u8]> = out1.iter().map(|p| &*p.payload).collect();
        let payloads2: Vec<&[u8]> = out2.iter().map(|p| &*p.payload).collect();

        assert_eq!(
            payloads1,
            vec![b"chunk1" as &[u8], b"chunk2", b"chunk3", b"chunk4"]
        );
        assert_eq!(
            payloads2,
            vec![b"chunk1" as &[u8], b"chunk2", b"chunk3", b"chunk4"]
        );
    }

    #[tokio::test]
    async fn wait_for_data_unblocks_when_ring_is_cancelled() {
        let cancel = CancellationToken::new();
        let ts_ring = TsChunkRing::new(16, cancel.clone());
        let mut reader = TsChunkReader::new("reader".to_string(), &ts_ring);

        let wait_task = tokio::spawn(async move { reader.wait_for_data_or_cancelled().await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();

        assert_eq!(wait_task.await.unwrap(), TsChunkWaitResult::Cancelled);
    }
}
