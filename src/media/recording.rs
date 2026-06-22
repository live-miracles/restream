//! MKV recording muxer — writes live pipeline data to timestamped `.mkv` files.
//! Architecture: `RingBuffer` → `TsMuxer` → `MemoryQueue` → FFmpeg muxer on OS thread.
//! Auto-deletes recordings shorter than 5 seconds (transient connection artifacts).

use crate::media::codec::{audio_for_ts, video_for_ts};
use crate::media::engine::MediaEngine;
use crate::media::mpegts::TsMuxer;
use crate::media::ring_buffer::{DtsEnforcer, MediaType, Reader, RingBuffer};
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

    let queue = Arc::new(crate::media::avio::MemoryQueue::new());

    let queue_clone = queue.clone();
    let file_path_clone = file_path.clone();
    let cancel_token_clone = cancel_token.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_mkv_muxer(queue_clone, &file_path_clone, cancel_token_clone)
        }));
        match result {
            Ok(Err(e)) => eprintln!("[recording] MKV muxer failed: {:?}", e),
            Err(_) => eprintln!("[recording] MKV muxer panicked"),
            _ => {}
        }
    });

    let mut reader = Reader::new(format!("recording:{}", pipeline_name), ring_buffer);
    let mut packets = Vec::with_capacity(32);

    // Lazily initialized when first packet arrives
    let mut muxer: Option<TsMuxer> = None;
    let mut dts_enforcer: Option<DtsEnforcer> = None;
    let mut nalu_len_size: usize = 4;
    let mut audio_tracks: Vec<crate::media::engine::AudioMeta> = Vec::new();

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
                        // Lazily create the muxer from engine metadata
                        if muxer.is_none() {
                            let ingests = engine.active_ingests.read().await;
                            if let Some(ingest) = ingests.get(&pipeline_id) {
                                let video = ingest.video.as_ref();
                                let tracks = ingest.audio_tracks.lock().unwrap().clone();
                                let num_streams = video.is_some() as usize + tracks.len();
                                muxer = Some(TsMuxer::new(video, &tracks));
                                dts_enforcer = Some(DtsEnforcer::new(num_streams));
                                audio_tracks = tracks;
                                drop(ingests);
                            } else {
                                continue;
                            }
                        }

                        let Some(ref mut mux) = muxer else { continue };
                        let Some(ref mut dts) = dts_enforcer else { continue };

                        let payload = match pkt.media_type {
                            MediaType::Video => {
                                match video_for_ts(&pkt.payload, pkt.format, &mut nalu_len_size) {
                                    Some(p) => p,
                                    None => continue,
                                }
                            }
                            MediaType::Audio => {
                                let track = audio_tracks
                                    .iter()
                                    .find(|a| a.track_index == pkt.track_index)
                                    .or(audio_tracks.first());
                                let (sr, ch) = track
                                    .map(|a| (a.sample_rate, a.channels))
                                    .unwrap_or((48000, 1));
                                match audio_for_ts(&pkt.payload, pkt.format, sr, ch) {
                                    Some(p) => p,
                                    None => continue,
                                }
                            }
                        };

                        let stream_idx = match pkt.media_type {
                            MediaType::Video => 0,
                            MediaType::Audio => {
                                let video_offset = 1;
                                audio_tracks
                                    .iter()
                                    .position(|a| a.track_index == pkt.track_index)
                                    .map(|i| i + video_offset)
                                    .unwrap_or(0)
                            }
                        };

                        let (pts, dts) = dts.enforce(stream_idx, pkt.pts, pkt.dts);

                        let ts_bytes = mux.mux_packet(
                            pkt.media_type,
                            pkt.track_index,
                            pts,
                            dts,
                            pkt.is_keyframe,
                            &payload,
                        );

                        if !ts_bytes.is_empty() {
                            queue.write(ts_bytes);
                        }
                    }
                }
            }
        }
    }

    queue.close();

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
}

fn run_mkv_muxer(
    queue: Arc<crate::media::avio::MemoryQueue>,
    file_path: &str,
    token: CancellationToken,
) -> Result<(), &'static str> {
    use std::io::Write;

    let path = std::path::Path::new(file_path);
    let mut file = std::fs::File::create(path)
        .map_err(|_| "Recording: Failed to create output file")?;

    let mut buf = vec![0u8; 1316];
    let mut done = false;
    while !done {
        let n = queue.read(&mut buf);
        if n == 0 {
            if token.is_cancelled() {
                done = true;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
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
