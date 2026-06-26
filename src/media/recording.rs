//! MPEG-TS recording writer — writes live pipeline data to timestamped `.ts` files.
//! Architecture: `RingBuffer` → `TsMuxer` → `MemoryQueue` → raw TS byte writer on OS thread.
//! Auto-deletes recordings shorter than 5 seconds (transient connection artifacts).
//!
//! # Note on Container Format
//! The output is raw MPEG-TS (`.ts`), not Matroska/MKV. MPEG-TS is directly seekable
//! and playable by most media players and HLS-based workflows. A future upgrade to a
//! real container (e.g., MP4 with FFmpeg `avformat`) would require an avformat mux
//! context and is tracked as a roadmap item.

use crate::media::engine::MediaEngine;
use crate::media::feeder::{PacketFeedConfig, TsPacketFeeder};
use crate::media::ring_buffer::{Reader, RingBuffer};
use std::fs;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const MIN_DURATION_SECS: u64 = 5;

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

fn build_filename(pipe_name: &str) -> String {
    let now = chrono::Local::now();
    format!(
        "{} {}.ts",
        now.format("%Y-%m-%d %H-%M-%S"),
        sanitize_name(pipe_name)
    )
}

pub async fn start_recording(
    pipeline_name: String,
    pipeline_id: String,
    media_dir: String,
    ring_buffer: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel_token: CancellationToken,
) {
    let _ = fs::create_dir_all(&media_dir);
    let filename = build_filename(&pipeline_name);
    let file_path = format!("{}/{}", media_dir, filename);
    let started_at = std::time::Instant::now();

    println!("[recording] Started: {}", filename);

    let rec_stage_key = crate::domain::stage::StageKey::new(
        pipeline_id.as_str(),
        crate::domain::stage::StageKind::recording(),
    );
    let stage_metrics = engine
        .get_or_create_stage_metrics(rec_stage_key.clone())
        .await;
    engine
        .event_log
        .emit(crate::events::EventKind::StageStarted {
            pipeline_id: pipeline_id.clone(),
            encoding: "recording".to_string(),
        });

    let queue = Arc::new(crate::media::avio::MemoryQueue::new());

    // Guard: close the queue on drop so the OS writer thread always unblocks,
    // even if this async fn is cancelled or panics before reaching queue.close().
    struct QueueCloseGuard(Arc<crate::media::avio::MemoryQueue>);
    impl Drop for QueueCloseGuard {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _queue_guard = QueueCloseGuard(queue.clone());

    let queue_clone = queue.clone();
    let file_path_clone = file_path.clone();
    let cancel_token_clone = cancel_token.clone();
    // Store the JoinHandle so we can join the thread on exit and detect panics.
    // Dropping the handle detaches the thread silently — any crash becomes invisible.
    let muxer_handle = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_ts_writer(queue_clone, &file_path_clone, cancel_token_clone)
        }));
        match result {
            Ok(Err(e)) => eprintln!("[recording] TS writer failed: {:?}", e),
            Err(_) => eprintln!("[recording] TS writer panicked"),
            _ => {}
        }
    });

    let mut reader = Reader::new(format!("recording:{}", pipeline_name), ring_buffer);
    let mut packets = Vec::with_capacity(32);

    // Lazily initialized when first packet arrives.
    let (video_sequence_header, _) = engine.get_sequence_headers(&pipeline_id).await;
    let mut feeder: Option<TsPacketFeeder> = None;
    // Accumulation buffer: collect all muxed TS bytes for a burst, then
    // write them in a single queue.write() call (one lock acquisition per
    // burst instead of one per packet).
    let mut ts_batch: Vec<u8> = Vec::with_capacity(65536);

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

                    for pkt in &packets {
                        // Lazily create the feeder from engine metadata.
                        if feeder.is_none() {
                            let ingests = engine.active_ingests.read().await;
                            if let Some(ingest) = ingests.get(&pipeline_id) {
                                let video = ingest.video.as_ref();
                                let tracks = ingest.audio_tracks.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                feeder = Some(TsPacketFeeder::new(
                                    video,
                                    tracks,
                                    PacketFeedConfig {
                                        video_sequence_header: video_sequence_header.as_ref().map(|v| v.to_vec()),
                                        ..PacketFeedConfig::default()
                                    },
                                ));
                                drop(ingests);
                            } else {
                                continue;
                            }
                        }

                        if let Some(ref mut feeder) = feeder {
                            feeder.extend_ts_for_packet(pkt, &mut ts_batch);
                        }
                    }
                    // One lock acquisition for the whole burst.
                    if !ts_batch.is_empty() {
                        queue.write(&ts_batch).await;
                        ts_batch.clear();
                    }
                    for pkt in &packets {
                        stage_metrics.record_in(pkt.payload.len() as u64);
                    }
                }
            }
        }
    }

    queue.close();

    // Join the muxer thread to ensure the file is fully flushed before we
    // check the duration and potentially delete it.  Joining also surfaces
    // any panic that escaped catch_unwind (shouldn't happen, but be explicit).
    if let Err(e) = muxer_handle.join() {
        eprintln!(
            "[recording] TS writer thread join failed for {}: {:?}",
            filename, e
        );
    }

    let duration = started_at.elapsed();
    println!(
        "[recording] Ended: {} (duration: {:.1}s)",
        filename,
        duration.as_secs_f64()
    );

    if duration.as_secs() < MIN_DURATION_SECS {
        let _ = fs::remove_file(&file_path);
        println!("[recording] Deleted short recording: {}", filename);
    }

    engine.remove_stage_metrics(&rec_stage_key).await;
    engine
        .event_log
        .emit(crate::events::EventKind::StageStopped {
            pipeline_id: pipeline_id.clone(),
            encoding: "recording".to_string(),
        });
}

fn run_ts_writer(
    queue: Arc<crate::media::avio::MemoryQueue>,
    file_path: &str,
    // Cancellation propagates via queue.close() called by QueueCloseGuard on
    // the async side. The token is threaded through for future use (e.g., if
    // MemoryQueue gains a timed-read path) and to make the dependency explicit.
    _cancel: CancellationToken,
) -> Result<(), &'static str> {
    use std::io::Write;

    let path = std::path::Path::new(file_path);
    let mut file =
        std::fs::File::create(path).map_err(|_| "Recording: Failed to create output file")?;

    let mut buf = vec![0u8; 1316];
    let mut done = false;
    while !done {
        let n = queue.read(&mut buf);
        if n == 0 {
            done = true;
        } else {
            file.write_all(&buf[..n])
                .map_err(|_| "Recording: Failed to write")?;
        }
    }

    // Drain any remaining data after cancellation
    loop {
        let n = queue.read(&mut buf);
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|_| "Recording: Failed to write")?;
    }

    file.flush().map_err(|_| "Recording: Failed to flush")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::avio::MemoryQueue;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn run_ts_writer_exits_on_closed_queue() {
        let queue = Arc::new(MemoryQueue::new());
        queue.close();
        let token = CancellationToken::new();
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_recording.ts");
        let path_str = file_path.to_string_lossy().to_string();

        let res = run_ts_writer(queue, &path_str, token);
        assert!(res.is_ok());
        let _ = std::fs::remove_file(file_path);
    }

    #[test]
    fn sanitize_name_replaces_path_chars() {
        assert_eq!(
            sanitize_name("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
    }

    #[test]
    fn sanitize_name_preserves_alphanumeric_and_dashes() {
        assert_eq!(sanitize_name("My-Pipeline_v2"), "My-Pipeline_v2");
    }

    #[test]
    fn sanitize_name_empty_string() {
        assert_eq!(sanitize_name(""), "");
    }

    #[test]
    fn build_filename_has_ts_extension() {
        let name = build_filename("test-pipe");
        assert!(name.ends_with(".ts"));
    }

    #[test]
    fn build_filename_contains_sanitized_name() {
        let name = build_filename("My Pipe?");
        assert!(
            name.contains("My Pipe_"),
            "expected sanitized name in: {name}"
        );
    }

    #[test]
    fn ts_writer_writes_data_to_file() {
        let queue = Arc::new(MemoryQueue::new());
        let token = CancellationToken::new();
        let temp = std::env::temp_dir().join("test_write.ts");
        let path = temp.to_string_lossy().to_string();
        queue.write_sync(b"hello world");
        queue.close();
        let res = run_ts_writer(queue, &path, token);
        assert!(res.is_ok());
        let content = std::fs::read(&temp).unwrap();
        assert_eq!(content, b"hello world");
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn ts_writer_empty_closed_queue_creates_empty_file() {
        let queue = Arc::new(MemoryQueue::new());
        queue.close();
        let token = CancellationToken::new();
        let temp = std::env::temp_dir().join("test_empty.ts");
        let path = temp.to_string_lossy().to_string();
        assert!(run_ts_writer(queue, &path, token).is_ok());
        assert_eq!(std::fs::read(&temp).unwrap().len(), 0);
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn ts_writer_fails_on_invalid_path() {
        let queue = Arc::new(MemoryQueue::new());
        let token = CancellationToken::new();
        assert!(run_ts_writer(queue, "/nonexistent_dir/should/fail.ts", token).is_err());
    }

    // H5: QueueCloseGuard must unblock the writer thread even if the queue is
    // never explicitly closed by the caller (e.g., async fn cancelled/panicked).
    // Simulate by dropping the guard and verifying the writer exits.
    #[test]
    fn queue_close_guard_unblocks_writer_thread() {
        let queue = Arc::new(MemoryQueue::new());

        // Start the writer thread on an open queue.
        let queue_for_thread = queue.clone();
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_guard_recording.ts");
        let path_str = file_path.to_string_lossy().to_string();
        let token = CancellationToken::new();
        let thread = std::thread::spawn(move || run_ts_writer(queue_for_thread, &path_str, token));

        // Simulate the guard drop (async fn drop) by closing the queue directly.
        // In production this is done by QueueCloseGuard::drop.
        queue.close();

        // Writer thread must exit within 1 second — no hang.
        let result = thread.join().expect("writer thread panicked");
        assert!(result.is_ok());
        let _ = std::fs::remove_file(temp_dir.join("test_guard_recording.ts"));
    }
}
