//! Offline media-file inspection helpers used to validate ingest suitability
//! and expose operator diagnostics for stored media assets.

use ffmpeg_next::{format, media};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const LIVE_GOP_WARNING_THRESHOLD_SECS: f64 = 2.0;
pub const DEFAULT_LIVE_GOP_TARGET_SECONDS: u32 = 2;
const LIVE_GOP_WARNING_TOLERANCE_SECS: f64 = 0.05;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFileAnalysis {
    pub video_codec: Option<String>,
    pub fps: Option<f64>,
    pub duration_sec: Option<f64>,
    pub keyframe_count: usize,
    pub average_keyframe_interval_sec: Option<f64>,
    pub max_keyframe_interval_sec: Option<f64>,
    pub sparse_for_live: bool,
    pub live_gop_target_seconds: u32,
}

fn round_metric(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn is_sparse_gop_interval(max_interval_sec: f64) -> bool {
    max_interval_sec > LIVE_GOP_WARNING_THRESHOLD_SECS + LIVE_GOP_WARNING_TOLERANCE_SECS
}

fn codec_name(id: ffmpeg_next::codec::Id) -> String {
    match id {
        ffmpeg_next::codec::Id::H264 => "h264",
        ffmpeg_next::codec::Id::HEVC => "hevc",
        ffmpeg_next::codec::Id::AAC => "aac",
        other => return format!("{other:?}").to_ascii_lowercase(),
    }
    .to_string()
}

fn timestamp_seconds(
    stream: &ffmpeg_next::Stream<'_>,
    packet: &ffmpeg_next::Packet,
) -> Option<f64> {
    let ts = packet.dts().or_else(|| packet.pts())?;
    let tb = stream.time_base();
    if tb.1 == 0 {
        return Some(ts as f64);
    }
    Some(ts as f64 * tb.0 as f64 / tb.1 as f64)
}

pub fn analyze_media_file(path: &Path) -> Result<MediaFileAnalysis, String> {
    let mut ictx =
        format::input(path).map_err(|error| format!("Failed to open media file: {error}"))?;
    let Some(video_stream) = ictx.streams().best(media::Type::Video).or_else(|| {
        ictx.streams()
            .find(|stream| stream.parameters().medium() == media::Type::Video)
    }) else {
        return Ok(MediaFileAnalysis {
            video_codec: None,
            fps: None,
            duration_sec: None,
            keyframe_count: 0,
            average_keyframe_interval_sec: None,
            max_keyframe_interval_sec: None,
            sparse_for_live: false,
            live_gop_target_seconds: DEFAULT_LIVE_GOP_TARGET_SECONDS,
        });
    };

    let video_index = video_stream.index();
    let video_codec = Some(codec_name(video_stream.parameters().id()));
    let frame_rate = video_stream.avg_frame_rate();
    let fps = if frame_rate.0 > 0 && frame_rate.1 > 0 {
        Some(round_metric(frame_rate.0 as f64 / frame_rate.1 as f64))
    } else {
        None
    };
    let duration_sec = if ictx.duration() > 0 {
        Some(round_metric(ictx.duration() as f64 / 1_000_000.0))
    } else {
        None
    };

    let mut keyframe_times = Vec::new();
    for (stream, packet) in ictx.packets() {
        if stream.index() != video_index || !packet.is_key() {
            continue;
        }
        if let Some(timestamp) = timestamp_seconds(&stream, &packet) {
            keyframe_times.push(timestamp);
        }
    }

    let (average_keyframe_interval_sec, max_keyframe_interval_sec, sparse_for_live) =
        if keyframe_times.len() >= 2 {
            let intervals: Vec<f64> = keyframe_times
                .windows(2)
                .map(|window| (window[1] - window[0]).max(0.0))
                .collect();
            let avg = intervals.iter().sum::<f64>() / intervals.len() as f64;
            let max = intervals.iter().copied().fold(0.0f64, f64::max);
            (
                Some(round_metric(avg)),
                Some(round_metric(max)),
                is_sparse_gop_interval(max),
            )
        } else {
            (None, None, false)
        };

    Ok(MediaFileAnalysis {
        video_codec,
        fps,
        duration_sec,
        keyframe_count: keyframe_times.len(),
        average_keyframe_interval_sec,
        max_keyframe_interval_sec,
        sparse_for_live,
        live_gop_target_seconds: DEFAULT_LIVE_GOP_TARGET_SECONDS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_fixture_reports_2_second_gop() {
        let fixture =
            crate::test_fixtures::canonical_h264_ts_fixture().expect("fixture should exist");
        let analysis = analyze_media_file(&fixture).expect("analysis should succeed");

        assert_eq!(analysis.video_codec.as_deref(), Some("h264"));
        assert_eq!(analysis.keyframe_count, 4);
        assert_eq!(analysis.average_keyframe_interval_sec, Some(2.0));
        assert_eq!(analysis.max_keyframe_interval_sec, Some(2.0));
        assert!(!analysis.sparse_for_live);
    }

    #[test]
    fn sparse_fixture_reports_sparse_gop() {
        let fixture = crate::test_fixtures::sparse_gop_mp4_fixture().expect("fixture should exist");
        let analysis = analyze_media_file(&fixture).expect("analysis should succeed");

        assert_eq!(analysis.video_codec.as_deref(), Some("h264"));
        assert_eq!(analysis.keyframe_count, 3);
        assert_eq!(analysis.average_keyframe_interval_sec, Some(5.0));
        assert_eq!(analysis.max_keyframe_interval_sec, Some(5.0));
        assert!(analysis.sparse_for_live);
    }

    #[test]
    fn sparse_gop_threshold_uses_small_tolerance() {
        assert!(!is_sparse_gop_interval(2.0));
        assert!(!is_sparse_gop_interval(2.03));
        assert!(is_sparse_gop_interval(2.25));
        assert!(is_sparse_gop_interval(5.0));
    }
}
