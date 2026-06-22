//! Payload format conversions for the 2×3×2 ingest/egress matrix.
//!
//! Four entry points cover every path:
//!   - `video_for_ts` / `audio_for_ts`  — prepare payloads for MPEG-TS muxing (SRT/HLS egress, transcoder feeder)
//!   - `video_for_rtmp` / `audio_for_rtmp` — prepare payloads for RTMP publishing
//!
//! Lower-level helpers (`avcc_to_annexb`, `annexb_to_avcc`, etc.) are also public
//! for use in sequence header synthesis and tests.

use bytes::Bytes;
use std::borrow::Cow;

use crate::media::ring_buffer::PayloadFormat;

// ---------------------------------------------------------------------------
// High-level: payload → MPEG-TS ready
// ---------------------------------------------------------------------------

/// Prepare a video payload for MPEG-TS muxing (Annex B output).
///
/// - **FLV**: strips 5-byte header; sequence headers (packet_type 0) update
///   `*nalu_len_size` and `*sps_pps_cache` (does NOT emit a standalone packet,
///   returns None); data keyframes prepend cached SPS/PPS then AVCC→Annex B;
///   non-keyframes convert AVCC→Annex B.
/// - **Raw**: pass-through (already Annex B with inline SPS/PPS).
pub fn video_for_ts<'a>(
    payload: &'a [u8],
    format: PayloadFormat,
    nalu_len_size: &mut usize,
    sps_pps_cache: &mut Vec<u8>,
) -> Option<Cow<'a, [u8]>> {
    match format {
        PayloadFormat::Raw => {
            if payload.is_empty() {
                None
            } else {
                Some(Cow::Borrowed(payload))
            }
        }
        PayloadFormat::Flv => {
            if payload.len() <= 5 {
                return None;
            }
            if payload[1] == 0 {
                // Sequence header — cache SPS/PPS Annex B for inline injection
                let (nls, annexb) = parse_avcc_config(&payload[5..]);
                *nalu_len_size = nls;
                *sps_pps_cache = annexb;
                // Don't emit a standalone packet; SPS/PPS will be prepended to IDR frames
                None
            } else {
                let is_keyframe = (payload[0] & 0xF0) == 0x10;
                let annexb = avcc_to_annexb(&payload[5..], *nalu_len_size);
                if annexb.is_empty() {
                    return None;
                }
                if is_keyframe && !sps_pps_cache.is_empty() {
                    // Prepend SPS/PPS inline before IDR so every keyframe is self-contained
                    let mut out = sps_pps_cache.clone();
                    out.extend_from_slice(&annexb);
                    Some(Cow::Owned(out))
                } else {
                    Some(Cow::Owned(annexb))
                }
            }
        }
    }
}

/// Prepare an audio payload for MPEG-TS muxing (ADTS-wrapped output).
///
/// - **FLV**: strips 2-byte header, skips config packets (packet_type 0),
///   prepends a 7-byte ADTS header to the raw AAC frame.
/// - **Raw with ADTS** (from SRT ingest): pass-through.
/// - **Raw without ADTS** (from transcoder/FFmpeg): prepends ADTS header.
pub fn audio_for_ts<'a>(
    payload: &'a [u8],
    format: PayloadFormat,
    sample_rate: u32,
    channels: u32,
) -> Option<Cow<'a, [u8]>> {
    match format {
        PayloadFormat::Raw => {
            if payload.is_empty() {
                return None;
            }
            if has_adts_sync(payload) {
                Some(Cow::Borrowed(payload))
            } else {
                Some(Cow::Owned(prepend_adts(payload, sample_rate, channels)))
            }
        }
        PayloadFormat::Flv => {
            if payload.len() <= 2 || payload[1] == 0 {
                return None;
            }
            let raw_aac = &payload[2..];
            Some(Cow::Owned(prepend_adts(raw_aac, sample_rate, channels)))
        }
    }
}

// ---------------------------------------------------------------------------
// High-level: payload → RTMP/FLV ready
// ---------------------------------------------------------------------------

/// Prepare a Raw (Annex B) video payload for RTMP publishing.
///
/// Converts Annex B → AVCC, wraps in 5-byte FLV video tag header.
/// Returns `None` if the converted payload is empty.
pub fn video_for_rtmp(payload: &[u8], is_keyframe: bool) -> Option<Vec<u8>> {
    let avcc_data = annexb_to_avcc(payload);
    if avcc_data.is_empty() {
        return None;
    }
    let tag = if is_keyframe { 0x17u8 } else { 0x27u8 };
    let mut out = Vec::with_capacity(avcc_data.len() + 5);
    out.extend_from_slice(&[tag, 1, 0, 0, 0]);
    out.extend_from_slice(&avcc_data);
    Some(out)
}

/// Prepare a Raw audio payload for RTMP publishing.
///
/// Strips ADTS header if present, prepends 2-byte FLV audio header `[0xAF, 0x01]`.
pub fn audio_for_rtmp(payload: &[u8]) -> Vec<u8> {
    let raw = strip_adts(payload);
    let mut out = Vec::with_capacity(raw.len() + 2);
    out.extend_from_slice(&[0xAF, 0x01]);
    out.extend_from_slice(raw);
    out
}

// ---------------------------------------------------------------------------
// AVCC ↔ Annex B conversion
// ---------------------------------------------------------------------------

/// Parse AVCC decoder configuration record.
/// Returns `(nalu_length_size, sps_pps_as_annexb)`.
pub fn parse_avcc_config(data: &[u8]) -> (usize, Vec<u8>) {
    if data.len() < 8 {
        return (4, Vec::new());
    }
    let nalu_len_size = ((data[4] & 0x03) + 1) as usize;
    let mut out = Vec::new();
    let num_sps = (data[5] & 0x1F) as usize;
    let mut pos = 6;
    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            return (nalu_len_size, out);
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            return (nalu_len_size, out);
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    if pos >= data.len() {
        return (nalu_len_size, out);
    }
    let num_pps = data[pos] as usize;
    pos += 1;
    for _ in 0..num_pps {
        if pos + 2 > data.len() {
            return (nalu_len_size, out);
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            return (nalu_len_size, out);
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    (nalu_len_size, out)
}

/// Convert AVCC-format NALUs to Annex B (start codes).
pub fn avcc_to_annexb(data: &[u8], nalu_len_size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos + nalu_len_size <= data.len() {
        let nalu_len = match nalu_len_size {
            1 => data[pos] as usize,
            2 => u16::from_be_bytes([data[pos], data[pos + 1]]) as usize,
            3 => {
                ((data[pos] as usize) << 16)
                    | ((data[pos + 1] as usize) << 8)
                    | (data[pos + 2] as usize)
            }
            _ => u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize,
        };
        pos += nalu_len_size;
        if nalu_len == 0 || pos + nalu_len > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + nalu_len]);
        pos += nalu_len;
    }
    out
}

/// Convert Annex B NALUs to AVCC format (4-byte length prefix).
/// Filters out SPS (7), PPS (8), and AUD (9) NALUs.
pub fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let nalus = split_annexb_nalus(data);
    let mut out = Vec::with_capacity(data.len());
    for nalu in &nalus {
        if nalu.is_empty() {
            continue;
        }
        let nal_type = nalu[0] & 0x1F;
        if nal_type == 7 || nal_type == 8 || nal_type == 9 {
            continue;
        }
        let len = nalu.len() as u32;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nalu);
    }
    out
}

/// Locate all Annex B start codes (`0x00 0x00 0x01` and `0x00 0x00 0x00 0x01`).
/// Returns a list of `(start_index, end_index)` spans of the start codes themselves.
pub fn find_annexb_start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    let finder = memchr::memmem::Finder::new(&[0, 0, 1]);
    for idx in finder.find_iter(data) {
        let mut start = idx;
        while start > 0 && data[start - 1] == 0 {
            start -= 1;
        }
        let sc_len = idx - start + 3;
        matches.push((start, start + sc_len));
    }
    matches
}

/// Split Annex B byte stream into individual NALUs (without start codes).
pub fn split_annexb_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let starts = find_annexb_start_codes(data);
    for i in 0..starts.len() {
        let nalu_start = starts[i].1;
        let nalu_end = if i + 1 < starts.len() {
            starts[i + 1].0
        } else {
            data.len()
        };
        if nalu_start < nalu_end {
            nalus.push(&data[nalu_start..nalu_end]);
        }
    }
    nalus
}

/// Build an FLV video sequence header (AVCC decoder config) from Annex B keyframe data.
pub fn build_avcc_sequence_header(annexb_data: &[u8]) -> Option<Bytes> {
    let nalus = split_annexb_nalus(annexb_data);
    let sps_list: Vec<&[u8]> = nalus
        .iter()
        .filter(|n| !n.is_empty() && (n[0] & 0x1F) == 7)
        .copied()
        .collect();
    let pps_list: Vec<&[u8]> = nalus
        .iter()
        .filter(|n| !n.is_empty() && (n[0] & 0x1F) == 8)
        .copied()
        .collect();

    let sps = sps_list.first()?;
    if sps.len() < 4 {
        return None;
    }

    let mut buf = Vec::with_capacity(64);
    // FLV video tag: keyframe(0x17) + sequence header(0x00) + composition time(0,0,0)
    buf.extend_from_slice(&[0x17, 0x00, 0x00, 0x00, 0x00]);
    // AVCDecoderConfigurationRecord
    buf.push(1); // configurationVersion
    buf.push(sps[1]); // AVCProfileIndication
    buf.push(sps[2]); // profile_compatibility
    buf.push(sps[3]); // AVCLevelIndication
    buf.push(0xFF); // lengthSizeMinusOne = 3 (4 bytes)

    buf.push(0xE0 | sps_list.len() as u8);
    for s in &sps_list {
        buf.extend_from_slice(&(s.len() as u16).to_be_bytes());
        buf.extend_from_slice(s);
    }
    buf.push(pps_list.len() as u8);
    for p in &pps_list {
        buf.extend_from_slice(&(p.len() as u16).to_be_bytes());
        buf.extend_from_slice(p);
    }

    Some(Bytes::from(buf))
}

/// Build an FLV audio sequence header (AudioSpecificConfig) from sample rate
/// and channel count. Used for the SRT→RTMP Raw path where no cached
/// AudioSpecificConfig exists — the 2-byte config is synthesized from the
/// audio metadata that is always available.
pub fn build_aac_sequence_header(sample_rate: u32, channels: u32) -> Bytes {
    let freq_idx: u8 = match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        _ => 3,
    };
    let chan_cfg = channels.min(7) as u8;
    let audio_object_type: u8 = 2; // AAC-LC

    // AudioSpecificConfig (2 bytes for AAC-LC without extension)
    // byte0: bits[7:3] = audioObjectType, bits[2:0] = samplingFrequencyIndex top 3 bits
    let asc_byte0 = (audio_object_type << 3) | (freq_idx >> 1);
    // byte1: bit[7] = samplingFrequencyIndex bottom bit, bits[6:3] = channelConfiguration
    let asc_byte1 = ((freq_idx & 0x01) << 7) | (chan_cfg << 3);

    let mut out = Vec::with_capacity(4);
    // FLV audio tag: AAC (0xAF) + packet_type=0 (sequence header)
    out.extend_from_slice(&[0xAF, 0x00]);
    out.extend_from_slice(&[asc_byte0, asc_byte1]);
    Bytes::from(out)
}

// ---------------------------------------------------------------------------
// ADTS helpers
// ---------------------------------------------------------------------------

/// Build a 7-byte ADTS header for an AAC frame.
pub fn build_adts_header(frame_len: usize, sample_rate: u32, channels: u32) -> [u8; 7] {
    let freq_idx: u8 = match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        _ => 3,
    };
    let chan_cfg = channels.min(7) as u8;
    let total_len = (frame_len + 7) as u16;
    let mut hdr = [0u8; 7];
    hdr[0] = 0xFF;
    hdr[1] = 0xF1; // MPEG-4, Layer 0, no CRC
    hdr[2] = (1 << 6) | (freq_idx << 2) | (chan_cfg >> 2); // AAC-LC profile
    hdr[3] = ((chan_cfg & 0x03) << 6) | ((total_len >> 11) as u8 & 0x03);
    hdr[4] = (total_len >> 3) as u8;
    hdr[5] = ((total_len & 0x07) << 5) as u8 | 0x1F;
    hdr[6] = 0xFC;
    hdr
}

fn has_adts_sync(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xF0) == 0xF0
}

fn prepend_adts(raw_aac: &[u8], sample_rate: u32, channels: u32) -> Vec<u8> {
    let adts = build_adts_header(raw_aac.len(), sample_rate, channels);
    let mut out = Vec::with_capacity(7 + raw_aac.len());
    out.extend_from_slice(&adts);
    out.extend_from_slice(raw_aac);
    out
}

/// Strip ADTS header if present, returning the raw AAC frame data.
pub fn strip_adts(data: &[u8]) -> &[u8] {
    if has_adts_sync(data) && data.len() >= 7 {
        // protection_absent bit (byte 1, bit 0): 1 = no CRC (7-byte header), 0 = CRC (9-byte)
        let hdr_len = if data[1] & 0x01 == 1 { 7 } else { 9 };
        if data.len() > hdr_len {
            return &data[hdr_len..];
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_annexb_round_trip() {
        // SPS (type 7) + PPS (type 8) + IDR (type 5) as Annex B
        let annexb = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E, 0xAB, // SPS
            0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80, // PPS
            0, 0, 0, 1, 0x65, 0x88, 0x80, 0x40, // IDR slice
        ];

        // annexb_to_avcc should filter SPS/PPS/AUD and keep only IDR
        let avcc = annexb_to_avcc(&annexb);
        assert!(!avcc.is_empty());
        // First 4 bytes = length of the IDR NALU
        let nalu_len = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]) as usize;
        assert_eq!(nalu_len, 4); // IDR data: 0x65 0x88 0x80 0x40
        assert_eq!(avcc[4] & 0x1F, 5); // IDR NAL type

        // Convert back
        let back = avcc_to_annexb(&avcc, 4);
        assert_eq!(&back[..4], &[0, 0, 0, 1]); // start code
        assert_eq!(back[4] & 0x1F, 5); // IDR
    }

    #[test]
    fn parse_avcc_config_extracts_sps_pps() {
        // Minimal AVCC config: version=1, profile=66, compat=0, level=30, len_size=4
        let mut config = vec![
            1, 66, 0, 30, 0xFF, // lengthSizeMinusOne = 3 → 4 bytes
        ];
        // 1 SPS
        let sps = [0x67, 0x42, 0x00, 0x1E];
        config.push(0xE1); // num_sps = 1
        config.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        config.extend_from_slice(&sps);
        // 1 PPS
        let pps = [0x68, 0xCE, 0x38, 0x80];
        config.push(1); // num_pps = 1
        config.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        config.extend_from_slice(&pps);

        let (nls, annexb) = parse_avcc_config(&config);
        assert_eq!(nls, 4);
        // Should contain start_code + SPS + start_code + PPS
        assert!(annexb.len() > 8);
        assert_eq!(&annexb[..4], &[0, 0, 0, 1]);
        assert_eq!(annexb[4], 0x67); // SPS NAL type
    }

    #[test]
    fn adts_round_trip() {
        let raw_aac = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let with_adts = prepend_adts(&raw_aac, 48000, 2);
        assert_eq!(with_adts.len(), 7 + raw_aac.len());
        assert!(has_adts_sync(&with_adts));
        let stripped = strip_adts(&with_adts);
        assert_eq!(stripped, &raw_aac[..]);
    }

    #[test]
    fn video_for_ts_flv_passthrough_raw() {
        let annexb_payload = vec![0, 0, 0, 1, 0x65, 0x88];
        let mut nls = 4;
        let mut cache = Vec::new();
        let result = video_for_ts(&annexb_payload, PayloadFormat::Raw, &mut nls, &mut cache);
        assert!(result.is_some());
        // Raw should be zero-copy
        assert!(matches!(result, Some(Cow::Borrowed(_))));
        assert_eq!(&*result.unwrap(), &annexb_payload[..]);
    }

    #[test]
    fn audio_for_ts_adds_adts_for_raw_without() {
        let raw_aac = vec![0xDE, 0xAD];
        let result = audio_for_ts(&raw_aac, PayloadFormat::Raw, 48000, 2);
        assert!(result.is_some());
        let data = result.unwrap();
        assert!(has_adts_sync(&data));
        assert_eq!(&data[7..], &raw_aac[..]);
    }

    #[test]
    fn audio_for_ts_passes_through_existing_adts() {
        let mut with_adts = Vec::from(build_adts_header(4, 48000, 2));
        with_adts.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        let result = audio_for_ts(&with_adts, PayloadFormat::Raw, 48000, 2);
        assert!(matches!(result, Some(Cow::Borrowed(_))));
    }

    #[test]
    fn build_avcc_seq_header_from_annexb() {
        let annexb = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E, 0xAB, // SPS
            0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80, // PPS
            0, 0, 0, 1, 0x65, 0x88, 0x80, 0x40, // IDR
        ];
        let seq_hdr = build_avcc_sequence_header(&annexb).unwrap();
        // FLV tag: keyframe + seq header
        assert_eq!(seq_hdr[0], 0x17);
        assert_eq!(seq_hdr[1], 0x00);
        // AVCC config version
        assert_eq!(seq_hdr[5], 1);
    }

    #[test]
    fn video_for_rtmp_converts_annexb() {
        let annexb = [0, 0, 0, 1, 0x65, 0x88, 0x80, 0x40];
        let result = video_for_rtmp(&annexb, true).unwrap();
        assert_eq!(result[0], 0x17); // keyframe tag
        assert_eq!(result[1], 1); // data packet
        // AVCC data starts at offset 5
        let nalu_len = u32::from_be_bytes([result[5], result[6], result[7], result[8]]) as usize;
        assert_eq!(nalu_len, 4);
    }

    #[test]
    fn build_aac_seq_header_synthesizes_correct_config() {
        // AAC-LC (audioObjectType=2), 48000Hz (freq_idx=3), stereo (ch=2)
        // asc_byte0 = (2 << 3) | (3 >> 1) = 16 | 1 = 0x11
        // asc_byte1 = ((3 & 1) << 7) | (2 << 3) = 128 | 16 = 0x90
        let hdr = build_aac_sequence_header(48000, 2);
        assert_eq!(hdr.len(), 4);
        assert_eq!(hdr[0], 0xAF); // AAC, 44kHz, 16-bit, stereo
        assert_eq!(hdr[1], 0x00); // packet_type = 0 (sequence header)
        assert_eq!(hdr[2], 0x11);
        assert_eq!(hdr[3], 0x90);

        // AAC-LC, 44100Hz (freq_idx=4), mono (ch=1)
        // asc_byte0 = (2 << 3) | (4 >> 1) = 16 | 2 = 0x12
        // asc_byte1 = ((4 & 1) << 7) | (1 << 3) = 0 | 8 = 0x08
        let hdr2 = build_aac_sequence_header(44100, 1);
        assert_eq!(hdr2.len(), 4);
        assert_eq!(hdr2[0], 0xAF);
        assert_eq!(hdr2[1], 0x00);
        assert_eq!(hdr2[2], 0x12);
        assert_eq!(hdr2[3], 0x08);
    }

    #[test]
    fn audio_for_rtmp_strips_adts() {
        let raw = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut with_adts = Vec::from(build_adts_header(raw.len(), 48000, 2));
        with_adts.extend_from_slice(&raw);

        let result = audio_for_rtmp(&with_adts);
        assert_eq!(result[0], 0xAF);
        assert_eq!(result[1], 0x01);
        assert_eq!(&result[2..], &raw);
    }
}
