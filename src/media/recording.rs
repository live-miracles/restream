//! MKV recording muxer — writes live pipeline data to timestamped `.mkv` files.
//! Same architecture as HLS: `RingBuffer` → `MemoryQueue` → FFmpeg muxer on OS thread.
//! Auto-deletes recordings shorter than 5 seconds (transient connection artifacts).

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
        "{} {}.mkv",
        now.format("%Y-%m-%d %H-%M-%S"),
        sanitize_name(pipe_name)
    )
}

pub async fn start_recording(
    pipeline_name: String,
    media_dir: String,
    ring_buffer: Arc<RingBuffer>,
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

    let mut reader = Reader::new(ring_buffer);
    let mut packets = Vec::with_capacity(32);
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
                        queue.write(&packet.payload);
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
    use crate::media::avio::CustomInput;

    let mut custom_input = CustomInput::new(&*queue)?;
    let mut ictx = custom_input
        .input
        .take()
        .ok_or("Failed to get CustomInput context")?;

    let path = std::path::Path::new(file_path);
    let mut octx = ffmpeg_next::format::output_as(&path, "matroska")
        .map_err(|_| "Recording: Failed to open MKV output")?;

    let mut stream_mapping = Vec::new();
    for stream in ictx.streams() {
        let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::None);
        let mut new_stream = octx
            .add_stream(codec)
            .map_err(|_| "Recording: Failed to add stream")?;
        new_stream.set_parameters(stream.parameters());
        stream_mapping.push(new_stream.index());
    }

    octx.write_header()
        .map_err(|_| "Recording: Failed to write header")?;

    for (stream, mut packet) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }

        let Some(&out_stream_index) = stream_mapping.get(stream.index()) else {
            continue;
        };
        packet.set_stream(out_stream_index);

        let in_time_base = stream.time_base();
        let Some(out_stream) = octx.stream(out_stream_index) else {
            continue;
        };
        let out_time_base = out_stream.time_base();
        packet.rescale_ts(in_time_base, out_time_base);

        let _ = packet.write_interleaved(&mut octx);
    }

    octx.write_trailer()
        .map_err(|_| "Recording: Write trailer failed")?;
    Ok(())
}
