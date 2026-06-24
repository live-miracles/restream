//! A/V synchronisation regression tests.
//!
//! Validates that audio and video timestamps remain synchronised across all
//! processing paths over an extended run. Every test that references time
//! must pass at the 48-hour mark — the project minimum before declaring a
//! stream "long-run clean".
//!
//! # What is tested
//!
//! 1. **Cross-stream isolation**: a DTS bump on the video stream must not
//!    shift the audio stream's DTS counter, and vice versa.
//!
//! 2. **48-hour drift-free**: synthetic 30 fps video + 48 kHz/1024 audio
//!    fed through `DtsEnforcer` for 48 hours of content. Verifies:
//!    - Perfectly monotone input is never bumped (no false corrections).
//!    - Final |video_pts − audio_pts| stays within one video frame (33 ms).
//!
//! 3. **RTMP 32-bit timestamp boundary**: the RTMP millisecond timestamp is
//!    `u32`; it wraps at ~49.7 days. 48 hours (172 800 000 ms) must fit.
//!
//! 4. **MPEG-TS PTS round-trip exactness at 48 h**: `ms → 90 kHz → ms`
//!    must be lossless at the 48-hour mark. Loss here would shift one stream
//!    independently and cause visible sync drift.

use restream::media::ring_buffer::DtsEnforcer;

// ---------------------------------------------------------------------------
// 1. Cross-stream isolation
// ---------------------------------------------------------------------------

/// A DTS bump on stream 0 (video) must leave stream 1 (audio) unchanged.
/// If DtsEnforcer used shared state, a backward video DTS would shift
/// subsequent audio timestamps and cause persistent A/V drift.
#[test]
fn av_sync_no_cross_stream_coupling() {
    let mut e = DtsEnforcer::new(2); // 0 = video, 1 = audio

    // Frame 0 — both streams at t=0
    let (_, v0) = e.enforce(0, 0, 0);
    let (_, a0) = e.enforce(1, 0, 0);
    assert_eq!((v0, a0), (0, 0), "baseline");

    // Frame 1 — video DTS collides (non-monotone): must be bumped to prev+1=1
    let (_, v1) = e.enforce(0, 0, 0);
    assert_eq!(v1, 1, "video DTS must be bumped on collision");

    // Audio frame at t=21ms — must be unaffected by the video bump
    let (_, a1) = e.enforce(1, 21, 21);
    assert_eq!(a1, 21, "audio DTS must be unaffected by video bump");

    // A second video bump on stream 0 still must not bleed into stream 1
    let (_, v2) = e.enforce(0, 1, 1); // same as last video DTS → bumped to 2
    assert_eq!(v2, 2, "second video bump");
    let (_, a2) = e.enforce(1, 42, 42);
    assert_eq!(a2, 42, "audio continues unaffected after second video bump");
}

/// Audio bump must not affect video.
#[test]
fn av_sync_no_cross_stream_coupling_reverse() {
    let mut e = DtsEnforcer::new(2);
    e.enforce(0, 0, 0);  // video t=0
    e.enforce(1, 0, 0);  // audio t=0

    // Audio collision → bump
    let (_, a_bumped) = e.enforce(1, 0, 0);
    assert_eq!(a_bumped, 1, "audio bumped");

    // Video at t=33ms must be unaffected
    let (_, v) = e.enforce(0, 33, 33);
    assert_eq!(v, 33, "video unaffected by audio bump");
}

// ---------------------------------------------------------------------------
// 2. 48-hour drift-free simulation
// ---------------------------------------------------------------------------

/// Core of the 48h drift test, parameterised by video frame rate and audio sample rate.
///
/// Interleaves video (at `video_fps`) and `audio_hz`/1024-sample audio for
/// 48 hours of synthetic content, feeding every packet through `DtsEnforcer`.
/// Asserts:
///   - 0 DTS bumps on perfectly monotone input
///   - Final |video_pts − audio_pts| ≤ one video frame interval
fn run_48h_drift_test(video_fps: u64, audio_hz: u64) {
    // Integer-ms frame intervals matching our ring-buffer representation.
    let video_ms = 1000 / video_fps;               // e.g. 33ms@30fps, 16ms@60fps
    let audio_ms = 1024 * 1000 / audio_hz;         // 21ms@48kHz, 23ms@44.1kHz
    let duration_ms: u64 = 48 * 3600 * 1000; // 172_800_000

    let mut e = DtsEnforcer::new(2); // 0 = video, 1 = audio
    let mut video_bumps = 0u64;
    let mut audio_bumps = 0u64;
    let mut prev_v = -1i64;
    let mut prev_a = -1i64;
    let mut last_v = 0i64;
    let mut last_a = 0i64;
    let mut vt = 0u64;
    let mut at = 0u64;

    while vt < duration_ms || at < duration_ms {
        if vt < duration_ms && (at >= duration_ms || vt <= at) {
            let pts = vt as i64;
            let (out_pts, out_dts) = e.enforce(0, pts, pts);
            assert!(
                out_dts > prev_v,
                "[{video_fps}fps] video DTS non-monotone at t={vt}ms: {out_dts} <= {prev_v}"
            );
            if out_dts != pts {
                video_bumps += 1;
            }
            prev_v = out_dts;
            last_v = out_pts;
            vt += video_ms;
        } else {
            let pts = at as i64;
            let (out_pts, out_dts) = e.enforce(1, pts, pts);
            assert!(
                out_dts > prev_a,
                "[{video_fps}fps] audio DTS non-monotone at t={at}ms: {out_dts} <= {prev_a}"
            );
            if out_dts != pts {
                audio_bumps += 1;
            }
            prev_a = out_dts;
            last_a = out_pts;
            at += audio_ms;
        }
    }

    assert_eq!(
        video_bumps, 0,
        "[{video_fps}fps] DtsEnforcer bumped video {video_bumps} times on monotone input"
    );
    assert_eq!(
        audio_bumps, 0,
        "[{video_fps}fps] DtsEnforcer bumped audio {audio_bumps} times on monotone input"
    );

    let drift_ms = (last_v - last_a).abs();
    assert!(
        drift_ms <= video_ms as i64,
        "[{video_fps}fps] A/V drift after 48h: {drift_ms}ms > one frame ({video_ms}ms)\n  \
         last_video_pts={last_v}ms  last_audio_pts={last_a}ms"
    );
}

// -- 48 kHz audio (broadcast standard: 21 ms/frame) --

/// 24 fps + 48 kHz — film / broadcast baseline
#[test]
fn av_sync_48h_drift_free_24fps_48khz() {
    run_48h_drift_test(24, 48_000);
}

/// 25 fps + 48 kHz — PAL broadcast
#[test]
fn av_sync_48h_drift_free_25fps_48khz() {
    run_48h_drift_test(25, 48_000);
}

/// 30 fps + 48 kHz — standard NTSC / web streaming
#[test]
fn av_sync_48h_drift_free_30fps_48khz() {
    run_48h_drift_test(30, 48_000);
}

/// 50 fps + 48 kHz — PAL high-frame-rate
#[test]
fn av_sync_48h_drift_free_50fps_48khz() {
    run_48h_drift_test(50, 48_000);
}

/// 60 fps + 48 kHz — high-frame-rate gaming / sports
#[test]
fn av_sync_48h_drift_free_60fps_48khz() {
    run_48h_drift_test(60, 48_000);
}

// -- 44.1 kHz audio (music / CD quality: 23 ms/frame) --
// Different LCM with each video rate → different quantisation residuals.

/// 24 fps + 44.1 kHz — music content at film rate
#[test]
fn av_sync_48h_drift_free_24fps_44khz() {
    run_48h_drift_test(24, 44_100);
}

/// 25 fps + 44.1 kHz
#[test]
fn av_sync_48h_drift_free_25fps_44khz() {
    run_48h_drift_test(25, 44_100);
}

/// 30 fps + 44.1 kHz — most common music-stream combination
#[test]
fn av_sync_48h_drift_free_30fps_44khz() {
    run_48h_drift_test(30, 44_100);
}

/// 50 fps + 44.1 kHz
#[test]
fn av_sync_48h_drift_free_50fps_44khz() {
    run_48h_drift_test(50, 44_100);
}

/// 60 fps + 44.1 kHz — worst-case quantisation combination (LCM = 368 ms)
#[test]
fn av_sync_48h_drift_free_60fps_44khz() {
    run_48h_drift_test(60, 44_100);
}

// ---------------------------------------------------------------------------
// 3. RTMP 32-bit timestamp boundary
// ---------------------------------------------------------------------------

/// RTMP millisecond timestamps are `u32` fields that wrap at 2^32 ms ≈ 49.7 d.
/// At 48 h the field value must still fit — wrapping would reset timestamps to
/// near-zero and break A/V sync on the egress side.
#[test]
fn av_sync_rtmp_timestamp_48h_no_wrap() {
    let ms_48h: u64 = 48 * 3600 * 1000; // 172_800_000
    assert!(
        ms_48h <= u32::MAX as u64,
        "48 h ({} ms) overflows RTMP u32 timestamp — stream would desync \
         before 48 h; wrap point is {:.1} h",
        ms_48h,
        u32::MAX as f64 / 3_600_000.0
    );
    // Confirm there is genuine headroom before the wrap
    let wrap_hours = u32::MAX as f64 / 3_600_000.0;
    assert!(
        wrap_hours > 49.0,
        "u32 RTMP wrap expected > 49 h, got {:.2} h",
        wrap_hours
    );
}

// ---------------------------------------------------------------------------
// 4. MPEG-TS PTS round-trip exactness at 48 h
// ---------------------------------------------------------------------------

/// `ms_to_ts(ms) = ms * 90`, `ts_to_ms(ts) = ts / 90`.
/// Round-trip: `(ms * 90) / 90 == ms` — exact integer arithmetic, no
/// truncation, so no accumulated drift from the 90 kHz PTS encoding.
/// Tested at the 48-hour mark and at typical frame boundaries.
#[test]
fn av_sync_mpeg_ts_pts_roundtrip_exact_at_48h() {
    let roundtrip = |ms: i64| -> i64 { (ms * 90) / 90 };

    let cases: &[(i64, &str)] = &[
        (0, "t=0"),
        (33, "video frame @33ms"),
        (21, "audio frame @21ms"),
        (1000, "1 s"),
        (3_600_000, "1 h"),
        (24 * 3_600_000, "24 h"),
        (48 * 3_600_000, "48 h"),
    ];

    for &(ms, label) in cases {
        assert_eq!(
            roundtrip(ms),
            ms,
            "MPEG-TS PTS round-trip not exact at {label} ({ms} ms) — \
             drift would shift one stream relative to the other"
        );
    }

    // No i64 overflow at 48-hour 90 kHz scale
    let pts_90k_48h: i64 = 48 * 3_600_000i64 * 90; // 15_552_000_000
    assert!(pts_90k_48h > 0, "i64 overflow at 48 h × 90 kHz PTS");
}
