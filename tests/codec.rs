//! Correctness tests for `media::codec` — the payload conversion functions
//! that handle every cell of the 2×3×2 ingest/egress matrix.
//!
//! These tests exercise the *runtime* conversion path (the actual functions
//! called in SRT/HLS egress and the transcoder feeder), not a hand-rolled copy.

use restream::media::codec::{
    audio_for_rtmp, audio_for_ts, avcc_to_annexb, build_adts_header, build_avcc_sequence_header,
    parse_avcc_config, strip_adts, video_for_rtmp, video_for_ts,
};
use restream::media::engine::{AudioMeta, VideoMeta};
use restream::media::mpegts::TsMuxer;
use restream::media::ring_buffer::{MediaType, PayloadFormat};

// ---------------------------------------------------------------------------
// Minimal test fixtures
// ---------------------------------------------------------------------------

/// Minimal SPS NALU (H.264 Baseline profile, level 3.0, 320×240).
/// These bytes are a real SPS extracted from an ffmpeg-generated stream.
const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0xa0, 0x47, 0xfe, 0xc8];
const PPS: &[u8] = &[0x68, 0xce, 0x38, 0x80];
/// Minimal IDR slice header bytes (not a full frame, but sufficient for type detection).
const IDR_NALU: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x23];

fn make_annexb(nalus: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nalus {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

fn make_flv_video_seq_header(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    // [frame_type|codec=0x17][packet_type=0][ct0,ct1,ct2=0][AVCDecoderConfigurationRecord]
    let mut buf = vec![0x17, 0x00, 0x00, 0x00, 0x00];
    // AVCDecoderConfigurationRecord
    buf.push(1); // version
    buf.push(sps[1]); // profile
    buf.push(sps[2]); // compat
    buf.push(sps[3]); // level
    buf.push(0xFF); // len_size_minus1 = 3 → 4 bytes
    buf.push(0xE1); // num_sps = 1
    buf.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    buf.extend_from_slice(sps);
    buf.push(1); // num_pps
    buf.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    buf.extend_from_slice(pps);
    buf
}

fn make_flv_video_data(nalu: &[u8], is_keyframe: bool, nalu_len_size: usize) -> Vec<u8> {
    let tag = if is_keyframe { 0x17u8 } else { 0x27u8 };
    let mut buf = vec![tag, 0x01, 0x00, 0x00, 0x00]; // packet_type=1 (NALU)
    // AVCC: length prefix + data
    let len = nalu.len();
    match nalu_len_size {
        1 => buf.push(len as u8),
        2 => buf.extend_from_slice(&(len as u16).to_be_bytes()),
        _ => buf.extend_from_slice(&(len as u32).to_be_bytes()),
    }
    buf.extend_from_slice(nalu);
    buf
}

fn make_flv_audio_seq(sample_rate: u32, channels: u32) -> Vec<u8> {
    // [sound_format=0xAF (AAC, 44kHz, stereo)][aac_packet_type=0 (AudioSpecificConfig)]
    // AudioSpecificConfig: minimal 2-byte version
    let freq_idx: u8 = match sample_rate {
        48000 => 3,
        44100 => 4,
        _ => 3,
    };
    let ch = channels.min(7) as u8;
    let asc1 = (0b00010 << 3) | (freq_idx >> 1); // AAC-LC profile=2, freq high bits
    let asc2 = ((freq_idx & 1) << 7) | (ch << 3);
    vec![0xAF, 0x00, asc1, asc2]
}

fn make_flv_audio_data(raw_aac: &[u8]) -> Vec<u8> {
    let mut buf = vec![0xAF, 0x01]; // AAC data packet
    buf.extend_from_slice(raw_aac);
    buf
}

// ---------------------------------------------------------------------------
// Unit tests: FLV → Annex B (video_for_ts)
// ---------------------------------------------------------------------------

#[test]
fn flv_seq_header_caches_sps_pps_and_returns_none() {
    let seq = make_flv_video_seq_header(SPS, PPS);
    let mut nls = 4usize;
    let mut cache = Vec::new();
    let result = video_for_ts(&seq, PayloadFormat::Flv, &mut nls, &mut cache);
    // Seq header is cached internally, not emitted as standalone packet
    assert!(result.is_none(), "seq header should be cached, not emitted");
    // nalu_len_size should be updated from the AVCC config (0xFF → 4 bytes)
    assert_eq!(nls, 4);
    // cache should now contain SPS/PPS as Annex B
    assert!(!cache.is_empty(), "SPS/PPS should be cached");
    assert_eq!(
        &cache[..4],
        &[0, 0, 0, 1],
        "cache starts with Annex B start code"
    );
    assert_eq!(cache[4] & 0x1F, 7, "first cached NALU should be SPS");
}

#[test]
fn flv_data_packet_converts_avcc_to_annexb() {
    let nalu_data = IDR_NALU;
    let flv_pkt = make_flv_video_data(nalu_data, true, 4);
    let mut nls = 4usize;
    let mut cache = Vec::new();
    let result = video_for_ts(&flv_pkt, PayloadFormat::Flv, &mut nls, &mut cache);
    assert!(result.is_some());
    let data = result.unwrap();
    assert_eq!(&data[..4], &[0, 0, 0, 1], "should have Annex B start code");
    assert_eq!(&data[4..], nalu_data, "NALU data should be preserved");
}

#[test]
fn raw_video_is_passthrough() {
    let annexb = make_annexb(&[IDR_NALU]);
    let mut nls = 4usize;
    let mut cache = Vec::new();
    let result = video_for_ts(&annexb, PayloadFormat::Raw, &mut nls, &mut cache);
    assert!(result.is_some());
    assert_eq!(&*result.unwrap(), &annexb[..]);
}

// ---------------------------------------------------------------------------
// Unit tests: FLV → ADTS (audio_for_ts)
// ---------------------------------------------------------------------------

#[test]
fn flv_audio_seq_is_skipped() {
    let seq = make_flv_audio_seq(48000, 2);
    let result = audio_for_ts(&seq, PayloadFormat::Flv, 48000, 2);
    assert!(result.is_none(), "FLV audio seq header should be skipped");
}

#[test]
fn flv_audio_data_gets_adts_header() {
    let raw_aac = &[0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c];
    let flv = make_flv_audio_data(raw_aac);
    let result = audio_for_ts(&flv, PayloadFormat::Flv, 48000, 2);
    assert!(result.is_some());
    let data = result.unwrap();
    // ADTS sync word
    assert_eq!(data[0], 0xFF);
    assert_eq!(data[1] & 0xF0, 0xF0);
    // Data after 7-byte header should be the raw AAC
    assert_eq!(&data[7..], raw_aac);
}

#[test]
fn raw_audio_with_adts_is_passthrough() {
    let adts = build_adts_header(4, 48000, 2);
    let mut payload = adts.to_vec();
    payload.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    let result = audio_for_ts(&payload, PayloadFormat::Raw, 48000, 2);
    assert!(result.is_some());
    assert_eq!(&*result.unwrap(), &payload[..]);
}

#[test]
fn raw_audio_without_adts_gets_header() {
    let raw = &[0x21, 0x10, 0x04, 0x60];
    let result = audio_for_ts(raw, PayloadFormat::Raw, 48000, 2);
    assert!(result.is_some());
    let data = result.unwrap();
    assert_eq!(data[0], 0xFF); // ADTS sync
    assert_eq!(&data[7..], raw);
}

// ---------------------------------------------------------------------------
// Unit tests: Annex B → RTMP (video_for_rtmp / audio_for_rtmp)
// ---------------------------------------------------------------------------

#[test]
fn annexb_keyframe_converts_to_flv_avcc() {
    let annexb = make_annexb(&[SPS, PPS, IDR_NALU]);
    let result = video_for_rtmp(&annexb, true);
    assert!(result.is_some());
    let data = result.unwrap();
    // FLV keyframe tag
    assert_eq!(data[0], 0x17, "should be keyframe tag");
    assert_eq!(data[1], 0x01, "should be data packet type");
    // AVCC data starts at offset 5: 4-byte length + NALU
    let nalu_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
    assert_eq!(nalu_len, IDR_NALU.len());
    assert_eq!(&data[9..9 + nalu_len], IDR_NALU);
}

#[test]
fn annexb_with_only_sps_pps_returns_none() {
    // SPS+PPS only → annexb_to_avcc filters them → empty → None
    let annexb = make_annexb(&[SPS, PPS]);
    let result = video_for_rtmp(&annexb, true);
    assert!(result.is_none(), "SPS/PPS-only frame should return None");
}

#[test]
fn audio_for_rtmp_strips_adts_adds_flv_header() {
    let raw_aac = &[0xDE, 0xAD, 0xBE, 0xEF];
    let adts = build_adts_header(raw_aac.len(), 48000, 2);
    let mut with_adts = adts.to_vec();
    with_adts.extend_from_slice(raw_aac);

    let result = audio_for_rtmp(&with_adts);
    assert_eq!(result[0], 0xAF);
    assert_eq!(result[1], 0x01);
    assert_eq!(&result[2..], raw_aac);
}

#[test]
fn audio_for_rtmp_raw_aac_no_adts() {
    // Raw AAC without ADTS should still get the 2-byte FLV header, data unchanged
    let raw_aac = &[0x21, 0x10, 0x04];
    let result = audio_for_rtmp(raw_aac);
    assert_eq!(result[0], 0xAF);
    assert_eq!(result[1], 0x01);
    assert_eq!(&result[2..], raw_aac);
}

#[test]
fn srt_demuxed_raw_packets_convert_to_rtmp_flv_packets() {
    let annexb = make_annexb(&[SPS, PPS, IDR_NALU]);
    let video_seq = build_avcc_sequence_header(&annexb).expect("AVCC sequence header");
    assert_eq!(video_seq[0], 0x17, "sequence header should be keyframe AVC");
    assert_eq!(video_seq[1], 0x00, "sequence header packet type");

    let video = video_for_rtmp(&annexb, true).expect("video data packet");
    assert_eq!(video[0], 0x17, "video should be an FLV keyframe packet");
    assert_eq!(video[1], 0x01, "video should be an AVC data packet");
    let nalu_len = u32::from_be_bytes([video[5], video[6], video[7], video[8]]) as usize;
    assert_eq!(nalu_len, IDR_NALU.len());
    assert_eq!(&video[9..9 + nalu_len], IDR_NALU);

    let raw_aac = &[0xDE, 0xAD, 0xBE, 0xEF];
    let mut adts_aac = build_adts_header(raw_aac.len(), 48000, 2).to_vec();
    adts_aac.extend_from_slice(raw_aac);
    let audio = audio_for_rtmp(&adts_aac);
    assert_eq!(audio[0], 0xAF, "audio should be AAC FLV");
    assert_eq!(audio[1], 0x01, "audio should be an AAC data packet");
    assert_eq!(&audio[2..], raw_aac);
}

// ---------------------------------------------------------------------------
// Integration: FLV packets → video_for_ts → TsMuxer → valid MPEG-TS output
// ---------------------------------------------------------------------------

#[test]
fn flv_video_through_ts_muxer_produces_valid_ts() {
    let video_meta = VideoMeta {
        codec: "h264".to_string(),
        width: 320,
        height: 240,
        ..Default::default()
    };
    let audio_meta = AudioMeta {
        codec: "aac".to_string(),
        sample_rate: 48000,
        channels: 2,
        track_index: 0,
        channel_layout: None,
        pid: None,
        language: None,
        title: None,
        profile: None,
    };
    let mut muxer = TsMuxer::new(Some(&video_meta), &[audio_meta]);
    let mut nls = 4usize;
    let mut cache = Vec::new();

    // Feed sequence header — should cache SPS/PPS and return None
    let seq = make_flv_video_seq_header(SPS, PPS);
    let seq_result = video_for_ts(&seq, PayloadFormat::Flv, &mut nls, &mut cache);
    assert!(
        seq_result.is_none(),
        "seq header should be cached, not emitted"
    );
    assert!(!cache.is_empty(), "SPS/PPS should be cached");

    // Feed an IDR frame — should include SPS/PPS inline + IDR Annex B
    let flv_idr = make_flv_video_data(IDR_NALU, true, nls);
    let idr_annexb = video_for_ts(&flv_idr, PayloadFormat::Flv, &mut nls, &mut cache).unwrap();
    // Should start with SPS start code (from cached SPS/PPS prepended before IDR)
    assert_eq!(
        &idr_annexb[..4],
        &[0, 0, 0, 1],
        "should start with Annex B start code"
    );
    assert_eq!(
        idr_annexb[4] & 0x1F,
        7,
        "first NALU should be SPS (from cache)"
    );
    let ts = muxer.mux_packet(MediaType::Video, 0, 0, 0, true, &idr_annexb);
    // Check MPEG-TS sync byte
    if !ts.is_empty() {
        assert_eq!(ts[0], 0x47, "MPEG-TS sync byte");
    }
}

// ---------------------------------------------------------------------------
// Round-trip: Annex B → video_for_rtmp → FLV AVCC → avcc_to_annexb → same data
// ---------------------------------------------------------------------------

#[test]
fn annexb_rtmp_round_trip() {
    let annexb = make_annexb(&[IDR_NALU]);
    let flv_wrapped = video_for_rtmp(&annexb, false).unwrap();

    // Strip the 5-byte FLV header to get raw AVCC
    let avcc_data = &flv_wrapped[5..];
    let back = avcc_to_annexb(avcc_data, 4);
    assert_eq!(&back[..4], &[0, 0, 0, 1]);
    assert_eq!(&back[4..], IDR_NALU);
}

// ---------------------------------------------------------------------------
// AVCC config parsing round-trip
// ---------------------------------------------------------------------------

#[test]
fn avcc_config_extracts_correct_nalu_len_size() {
    // Build an AVCC config with lengthSizeMinusOne = 3 (4-byte lengths)
    let seq = make_flv_video_seq_header(SPS, PPS);
    // seq[5..] is the AVCDecoderConfigurationRecord
    let (nls, annexb) = parse_avcc_config(&seq[5..]);
    assert_eq!(nls, 4);
    assert!(!annexb.is_empty());
    assert_eq!(
        annexb[4], SPS[0],
        "first NALU after start code should be SPS"
    );
}

// ---------------------------------------------------------------------------
// ADTS strip correctness
// ---------------------------------------------------------------------------

#[test]
fn strip_adts_removes_exactly_7_bytes_for_no_crc() {
    let raw = &[0xDE, 0xAD, 0xBE, 0xEF];
    let adts = build_adts_header(raw.len(), 44100, 1);
    assert_eq!(
        adts[1] & 0x01,
        1,
        "no-CRC bit should be set → 7-byte header"
    );
    let mut with_adts = adts.to_vec();
    with_adts.extend_from_slice(raw);
    assert_eq!(strip_adts(&with_adts), raw);
}

// ---------------------------------------------------------------------------
// build_avcc_sequence_header from Annex B
// ---------------------------------------------------------------------------

#[test]
fn build_seq_header_from_annexb_keyframe() {
    // Keyframe with SPS + PPS + IDR
    let annexb = make_annexb(&[SPS, PPS, IDR_NALU]);
    let hdr = build_avcc_sequence_header(&annexb).unwrap();
    // Bytes 0-4: FLV video tag (0x17=keyframe, 0x00=seq header, 0,0,0=ct)
    assert_eq!(hdr[0], 0x17);
    assert_eq!(hdr[1], 0x00);
    // Byte 5: AVCC config version = 1
    assert_eq!(hdr[5], 1);
    // Profile from SPS[1]
    assert_eq!(hdr[6], SPS[1]);
}
