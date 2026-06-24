use bytes::Bytes;

use crate::media::engine::{AudioMeta, VideoMeta};
use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat};
use memchr::memchr;

const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
const PAT_PID: u16 = 0x0000;
const PES_START_CODE: [u8; 3] = [0x00, 0x00, 0x01];
const MAX_PES_BUFFER: usize = 512 * 1024;
const PID_COUNT: usize = 1 << 13;
const NO_STREAM: u16 = u16::MAX;

// MPEG-TS stream type constants
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_H265: u8 = 0x24;
const STREAM_TYPE_AAC_ADTS: u8 = 0x0F;
const STREAM_TYPE_AAC_LATM: u8 = 0x11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    H264,
    H265,
    AacAdts,
    AacLatm,
}

impl StreamKind {
    fn from_stream_type(st: u8) -> Option<Self> {
        match st {
            STREAM_TYPE_H264 => Some(Self::H264),
            STREAM_TYPE_H265 => Some(Self::H265),
            STREAM_TYPE_AAC_ADTS => Some(Self::AacAdts),
            STREAM_TYPE_AAC_LATM => Some(Self::AacLatm),
            _ => None,
        }
    }

    fn media_type(self) -> MediaType {
        match self {
            Self::H264 | Self::H265 => MediaType::Video,
            Self::AacAdts | Self::AacLatm => MediaType::Audio,
        }
    }

    fn codec_name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "hevc",
            Self::AacAdts | Self::AacLatm => "aac",
        }
    }
}

#[derive(Debug)]
struct PesAccumulator {
    buf: Vec<u8>,
    pts: i64,
    dts: i64,
    has_timestamp: bool,
    random_access: bool,
}

impl PesAccumulator {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(16384),
            pts: 0,
            dts: 0,
            has_timestamp: false,
            random_access: false,
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.pts = 0;
        self.dts = 0;
        self.has_timestamp = false;
        self.random_access = false;
    }
}

#[derive(Debug)]
struct StreamInfo {
    /// The MPEG-TS elementary stream PID for this stream.
    /// Retained for future diagnostics — packet dispatch uses the
    /// `pid_to_stream` index table instead of a linear scan.
    _pid: u16,
    kind: StreamKind,
    track_index: u32,
    continuity: Option<u8>,
    pes: PesAccumulator,
}

/// Probe result matching the existing FFmpeg-based DemuxProbe.
#[derive(Debug, Clone)]
pub struct DemuxProbe {
    pub video: Option<VideoMeta>,
    pub audio_tracks: Vec<AudioMeta>,
}

/// Streaming MPEG-TS demuxer. Feed it chunks of TS data and drain packets.
pub struct TsDemuxer {
    streams: Vec<StreamInfo>,
    pid_to_stream: Box<[u16; PID_COUNT]>,
    pmt_pid: Option<u16>,
    probed: bool,
    probe_result: Option<DemuxProbe>,
    remainder: Vec<u8>,
    output: Vec<MediaPacket>,
    audio_track_counter: u32,
    probe_payloads: Vec<Option<Vec<u8>>>,
    pmt_buf: Vec<u8>,
    pmt_expected: usize,
    /// Tracks the last seen PMT version_number (bits 5–1 of the version/indicator byte).
    /// Used to distinguish genuine PMT updates from retransmissions of the same version.
    pmt_version: Option<u8>,
}

impl Default for TsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self {
            streams: Vec::new(),
            pid_to_stream: Box::new([NO_STREAM; PID_COUNT]),
            pmt_pid: None,
            probed: false,
            probe_result: None,
            remainder: Vec::new(),
            output: Vec::with_capacity(16),
            audio_track_counter: 0,
            probe_payloads: Vec::new(),
            pmt_buf: Vec::new(),
            pmt_expected: 0,
            pmt_version: None,
        }
    }

    /// Feed raw bytes (potentially multiple TS packets or partial ones).
    pub fn feed(&mut self, data: &[u8]) {
        if self.remainder.is_empty() {
            let leftover = self.feed_slice(data);
            if leftover < data.len() {
                self.remainder.extend_from_slice(&data[leftover..]);
            }
        } else {
            self.remainder.extend_from_slice(data);
            let buf = std::mem::take(&mut self.remainder);
            let leftover = self.feed_slice(&buf);
            if leftover < buf.len() {
                self.remainder.extend_from_slice(&buf[leftover..]);
            }
        }
        // Safety cap: remainder must never exceed TS_PACKET_SIZE-1 bytes.
        // feed_slice guarantees the unprocessed tail is < TS_PACKET_SIZE under
        // normal operation (it processes every complete 188-byte block it can).
        // This explicit cap prevents accumulation in edge cases — e.g. when
        // find_ts_sync optimistically accepts a 0x47 byte near the end of a
        // short chunk but the next chunk also starts with 0x47, causing the
        // buffer to grow one byte per call before the 188-byte threshold is
        // reached and the block is processed or discarded.
        const MAX_REMAINDER: usize = TS_PACKET_SIZE - 1;
        if self.remainder.len() > MAX_REMAINDER {
            let excess = self.remainder.len() - MAX_REMAINDER;
            self.remainder.drain(..excess);
        }
    }

    /// Process complete TS packets from a slice. Returns the offset of unconsumed bytes.
    fn feed_slice(&mut self, buf: &[u8]) -> usize {
        let mut offset = find_ts_sync(buf);

        while offset + TS_PACKET_SIZE <= buf.len() {
            if buf[offset] != TS_SYNC_BYTE {
                let next = find_ts_sync(&buf[offset + 1..]);
                offset += 1 + next;
                continue;
            }
            self.process_ts_packet(&buf[offset..offset + TS_PACKET_SIZE]);
            offset += TS_PACKET_SIZE;
        }

        offset
    }

    /// Drain completed media packets.
    pub fn drain(&mut self) -> Vec<MediaPacket> {
        std::mem::take(&mut self.output)
    }

    /// Move completed packets into a caller-owned reusable batch.
    ///
    /// Unlike `drain()`, this keeps the demuxer's output allocation available
    /// for subsequent receives. Callers should consume `output.drain(..)` to
    /// retain their batch allocation too.
    pub fn drain_into(&mut self, output: &mut Vec<MediaPacket>) -> usize {
        let start_len = output.len();
        output.append(&mut self.output);
        output.len() - start_len
    }

    /// Take the probe result (available after the first PMT + PES headers are parsed).
    pub fn take_probe(&mut self) -> Option<DemuxProbe> {
        self.probe_result.take()
    }

    /// Whether PMT has been parsed and streams are known.
    pub fn has_streams(&self) -> bool {
        !self.streams.is_empty()
    }

    fn process_ts_packet(&mut self, pkt: &[u8]) {
        let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        let payload_unit_start = pkt[1] & 0x40 != 0;
        let adaptation_field_control = (pkt[3] >> 4) & 0x03;
        let continuity_counter = pkt[3] & 0x0F;

        let mut payload_offset = 4;
        let mut random_access = false;

        // Adaptation field
        if (adaptation_field_control == 0x02 || adaptation_field_control == 0x03)
            && payload_offset < TS_PACKET_SIZE
        {
            let af_len = pkt[payload_offset] as usize;
            payload_offset += 1;
            if af_len > 0 && payload_offset < TS_PACKET_SIZE {
                let af_flags = pkt[payload_offset];
                random_access = af_flags & 0x40 != 0;
            }
            payload_offset += af_len;
        }

        // No payload
        if adaptation_field_control == 0x00 || adaptation_field_control == 0x02 {
            return;
        }

        if payload_offset >= TS_PACKET_SIZE {
            return;
        }

        let payload = &pkt[payload_offset..TS_PACKET_SIZE];

        if pid == PAT_PID {
            self.parse_pat(payload, payload_unit_start);
            return;
        }

        if Some(pid) == self.pmt_pid {
            self.parse_pmt(payload, payload_unit_start);
            return;
        }

        // MPEG-TS PIDs are 13-bit values, so direct dispatch avoids a linear
        // stream scan for every 188-byte packet.
        let stream_idx = self.pid_to_stream[pid as usize];
        if stream_idx == NO_STREAM {
            return;
        }
        let stream_idx = stream_idx as usize;

        // Continuity check (just track it, don't drop packets)
        self.streams[stream_idx].continuity = Some(continuity_counter);

        if payload_unit_start {
            // Flush previous PES before starting new one
            self.flush_pes(stream_idx);

            // Parse PES header
            if payload.len() >= 9 && payload[0..3] == PES_START_CODE {
                let pes_header_len = payload[8] as usize;
                let flags = payload[7];
                let has_pts = flags & 0x80 != 0;
                let has_dts = flags & 0x40 != 0;

                let stream = &mut self.streams[stream_idx];
                stream.pes.random_access = random_access;

                if has_pts && payload.len() >= 14 {
                    stream.pes.pts = parse_timestamp(&payload[9..14]);
                    stream.pes.has_timestamp = true;
                }
                if has_dts && payload.len() >= 19 {
                    stream.pes.dts = parse_timestamp(&payload[14..19]);
                } else if has_pts {
                    stream.pes.dts = stream.pes.pts;
                }

                let data_start = 9 + pes_header_len;
                if data_start < payload.len() {
                    let pes_data = &payload[data_start..];
                    if stream.pes.buf.len() + pes_data.len() <= MAX_PES_BUFFER {
                        stream.pes.buf.extend_from_slice(pes_data);
                    }
                }
            }
        } else {
            // Continuation of PES
            let stream = &mut self.streams[stream_idx];
            if stream.pes.buf.len() + payload.len() <= MAX_PES_BUFFER {
                stream.pes.buf.extend_from_slice(payload);
            }
        }
    }

    fn flush_pes(&mut self, stream_idx: usize) {
        let stream = &mut self.streams[stream_idx];
        if stream.pes.buf.is_empty() || !stream.pes.has_timestamp {
            stream.pes.reset();
            return;
        }

        let kind = stream.kind;
        let track_index = stream.track_index;
        let pts_90k = stream.pes.pts;
        let dts_90k = stream.pes.dts;
        let random_access = stream.pes.random_access;

        // Copy payload to a fresh Bytes, then reset the PES buffer keeping its
        // heap capacity for the next frame.  Using std::mem::take() would strip
        // the Vec capacity (leaving a 0-capacity Vec), forcing 3–8 reallocs on
        // the next PES reassembly. copy_from_slice costs one allocation of exactly
        // the frame size but keeps the PES buf warm — net saving for typical streams.
        let payload = Bytes::copy_from_slice(&stream.pes.buf);
        self.streams[stream_idx].pes.reset(); // clears buf, preserves capacity

        // Convert 90kHz to milliseconds
        let pts_ms = ts_to_ms(pts_90k);
        let dts_ms = ts_to_ms(dts_90k);

        let is_keyframe = match kind {
            StreamKind::H264 => random_access || h264_is_keyframe(&payload),
            StreamKind::H265 => random_access || h265_is_keyframe(&payload),
            _ => false,
        };

        // Build probe on first video/audio PES
        if !self.probed {
            self.try_build_probe(stream_idx, &payload);
        }

        self.output.push(MediaPacket {
            media_type: kind.media_type(),
            track_index,
            pts: pts_ms,
            dts: dts_ms,
            is_keyframe,
            format: PayloadFormat::Raw,
            payload,
        });
    }

    fn parse_pat(&mut self, payload: &[u8], pusi: bool) {
        let data = if pusi && !payload.is_empty() {
            let pointer = payload[0] as usize;
            if 1 + pointer >= payload.len() {
                return;
            }
            &payload[1 + pointer..]
        } else {
            payload
        };

        // PAT: table_id(1) + flags(2) + transport_stream_id(2) + version(1) + section(1) + last_section(1) + entries(4 each)
        if data.len() < 8 {
            return;
        }
        if data[0] != 0x00 {
            return; // table_id must be 0 for PAT
        }

        let section_length = ((data[1] as usize & 0x0F) << 8) | data[2] as usize;
        let end = (3 + section_length).min(data.len());

        // Skip 5 bytes (tsid + version + section numbers), then 4 bytes per entry, minus 4 byte CRC
        let mut pos = 8;
        while pos + 4 <= end.saturating_sub(4) {
            let program_num = ((data[pos] as u16) << 8) | data[pos + 1] as u16;
            let pid = ((data[pos + 2] as u16 & 0x1F) << 8) | data[pos + 3] as u16;
            if program_num != 0 {
                self.pmt_pid = Some(pid);
                break; // Single-program assumption
            }
            pos += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8], pusi: bool) {
        // No early-return on non-empty streams: we must process new PUSI packets
        // to detect PMT version changes (e.g., broadcaster adds an audio track).
        // Duplicate retransmissions of the same version are filtered below after
        // the full section is assembled.

        if pusi {
            let data = if !payload.is_empty() {
                let pointer = payload[0] as usize;
                if 1 + pointer >= payload.len() {
                    return;
                }
                &payload[1 + pointer..]
            } else {
                return;
            };

            if data.len() < 3 || data[0] != 0x02 {
                return;
            }
            let section_length = ((data[1] as usize & 0x0F) << 8) | data[2] as usize;
            self.pmt_expected = 3 + section_length;
            self.pmt_buf.clear();
            self.pmt_buf.extend_from_slice(data);
        } else if self.pmt_expected > 0 {
            self.pmt_buf.extend_from_slice(payload);
        } else {
            return;
        }

        if self.pmt_buf.len() < self.pmt_expected {
            return; // Need more continuation packets
        }

        let data = &self.pmt_buf;
        let end = self.pmt_expected.min(data.len());

        if data.len() < 12 {
            self.pmt_buf.clear();
            self.pmt_expected = 0;
            return;
        }

        // Check PMT version_number (ISO 13818-1 table syntax: byte 5 bits 5–1).
        // Skip retransmissions of the same version; reset stream state on change.
        let incoming_version = (data[5] >> 1) & 0x1F;
        if self.pmt_version == Some(incoming_version) {
            self.pmt_buf.clear();
            self.pmt_expected = 0;
            return; // Duplicate retransmission — nothing changed
        }
        self.pmt_version = Some(incoming_version);

        // Version changed (or first parse) — rebuild the stream map from the new PMT.
        //
        // For PIDs that survive unchanged into the new PMT we preserve their
        // in-flight PesAccumulator so that partially-assembled frames are not
        // lost mid-decode, which would cause a video glitch/audio pop until the
        // next IDR.  PIDs that disappear are simply dropped.  New PIDs get a
        // fresh accumulator.
        let mut old_pes: std::collections::HashMap<u16, PesAccumulator> =
            self.streams.drain(..).map(|s| (s._pid, s.pes)).collect();
        self.pid_to_stream.fill(NO_STREAM);
        self.audio_track_counter = 0;
        self.probe_payloads.clear();

        let program_info_len = ((data[10] as usize & 0x0F) << 8) | data[11] as usize;
        let mut pos = 12 + program_info_len;

        let mut has_video = false;
        while pos + 5 <= end.saturating_sub(4) {
            let stream_type = data[pos];
            let es_pid = ((data[pos + 1] as u16 & 0x1F) << 8) | data[pos + 2] as u16;
            let es_info_len = ((data[pos + 3] as usize & 0x0F) << 8) | data[pos + 4] as usize;
            pos += 5 + es_info_len;

            if let Some(kind) = StreamKind::from_stream_type(stream_type) {
                let track_index = match kind.media_type() {
                    MediaType::Video => {
                        if has_video {
                            continue; // Single video program
                        }
                        has_video = true;
                        0
                    }
                    MediaType::Audio => {
                        let idx = self.audio_track_counter;
                        self.audio_track_counter += 1;
                        idx
                    }
                };

                let stream_idx = self.streams.len();
                // Preserve in-flight PES accumulator for this PID if it existed
                // in the previous PMT — avoids dropping partially-assembled frames
                // when the broadcaster sends a PMT update that only changes metadata
                // (e.g. language descriptor) while keeping the same PIDs.
                let pes = old_pes.remove(&es_pid).unwrap_or_else(PesAccumulator::new);
                self.streams.push(StreamInfo {
                    _pid: es_pid,
                    kind,
                    track_index,
                    continuity: None,
                    pes,
                });
                self.pid_to_stream[es_pid as usize] = stream_idx as u16;
            }
        }

        self.pmt_buf.clear();
        self.pmt_expected = 0;
    }

    fn try_build_probe(&mut self, stream_idx: usize, payload: &[u8]) {
        // Stash payload for this stream's probe data
        if self.probe_payloads.len() < self.streams.len() {
            self.probe_payloads.resize(self.streams.len(), None);
        }
        if self.probe_payloads[stream_idx].is_none() {
            self.probe_payloads[stream_idx] = Some(payload.to_vec());
        }

        // Wait until all streams have contributed at least one PES
        if self.probe_payloads.iter().any(|p| p.is_none()) {
            return;
        }

        let mut video_meta = None;
        let mut audio_tracks = Vec::new();

        for (idx, stream) in self.streams.iter().enumerate() {
            let data = self.probe_payloads[idx].as_deref().unwrap();
            match stream.kind.media_type() {
                MediaType::Video => {
                    if video_meta.is_none() {
                        video_meta = Some(probe_video(stream.kind, data));
                    }
                }
                MediaType::Audio => {
                    audio_tracks.push(probe_audio(stream.kind, stream.track_index, data));
                }
            }
        }

        self.probed = true;
        self.probe_payloads.clear();
        self.probe_result = Some(DemuxProbe {
            video: video_meta,
            audio_tracks,
        });
    }

    /// Flush any remaining PES data for all streams (call at end of input).
    pub fn flush(&mut self) {
        for idx in 0..self.streams.len() {
            self.flush_pes(idx);
        }
    }
}

fn find_ts_sync(data: &[u8]) -> usize {
    if ts_sync_candidate_is_valid(data, 0) {
        return 0;
    }

    let mut search_offset = 0usize;
    while search_offset < data.len() {
        let Some(relative) = memchr(TS_SYNC_BYTE, &data[search_offset..]) else {
            return data.len();
        };
        let candidate = search_offset + relative;
        if ts_sync_candidate_is_valid(data, candidate) {
            return candidate;
        }
        search_offset = candidate + 1;
    }
    data.len()
}

fn ts_sync_candidate_is_valid(data: &[u8], candidate: usize) -> bool {
    if data.get(candidate) != Some(&TS_SYNC_BYTE) {
        return false;
    }

    let remaining = data.len() - candidate;
    if remaining <= TS_PACKET_SIZE {
        return true;
    }
    if data.get(candidate + TS_PACKET_SIZE) != Some(&TS_SYNC_BYTE) {
        return false;
    }
    remaining <= TS_PACKET_SIZE * 2
        || data.get(candidate + TS_PACKET_SIZE * 2) == Some(&TS_SYNC_BYTE)
}

// --- MPEG-TS muxer ---

/// MPEG-TS stream configuration for the muxer.
#[derive(Debug, Clone)]
pub struct MuxStreamConfig {
    pub stream_type: u8,
    pub pid: u16,
    pub media_type: MediaType,
    pub track_index: u32,
}

/// Streaming MPEG-TS muxer. Accepts MediaPackets and produces TS bytes.
pub struct TsMuxer {
    streams: Vec<MuxStreamConfig>,
    continuity: Vec<u8>,
    pat_cc: u8,
    pmt_cc: u8,
    pmt_pid: u16,
    pcr_pid: u16,
    last_pat_pmt_dts: Option<i64>,
    output: Vec<u8>,
}

impl TsMuxer {
    /// Create a new muxer from stream metadata.
    ///
    /// `flv_payloads`: if true, payloads have FLV wrappers that need stripping.
    pub fn new(video: Option<&VideoMeta>, audio_tracks: &[AudioMeta]) -> Self {
        let mut streams = Vec::new();
        let mut pid = 0x100u16;

        if let Some(v) = video {
            let stream_type = match v.codec.as_str() {
                "h264" => STREAM_TYPE_H264,
                "hevc" => STREAM_TYPE_H265,
                _ => STREAM_TYPE_H264,
            };
            streams.push(MuxStreamConfig {
                stream_type,
                pid,
                media_type: MediaType::Video,
                track_index: 0,
            });
            pid += 1;
        }

        for a in audio_tracks {
            let stream_type = match a.codec.as_str() {
                "aac" => STREAM_TYPE_AAC_ADTS,
                _ => STREAM_TYPE_AAC_ADTS,
            };
            streams.push(MuxStreamConfig {
                stream_type,
                pid,
                media_type: MediaType::Audio,
                track_index: a.track_index,
            });
            pid += 1;
        }

        let pcr_pid = streams.first().map_or(0x100, |s| s.pid);
        let continuity = vec![0u8; streams.len()];

        Self {
            streams,
            continuity,
            pat_cc: 0,
            pmt_cc: 0,
            pmt_pid: 0x1000,
            pcr_pid,
            last_pat_pmt_dts: None,
            output: Vec::with_capacity(TS_PACKET_SIZE * 8),
        }
    }

    /// Mux a MediaPacket into MPEG-TS bytes. Returns the produced bytes.
    ///
    /// `payload` should be the raw codec payload (FLV headers already stripped if needed).
    pub fn mux_packet(
        &mut self,
        media_type: MediaType,
        track_index: u32,
        pts_ms: i64,
        dts_ms: i64,
        is_keyframe: bool,
        payload: &[u8],
    ) -> &[u8] {
        self.output.clear();

        if payload.is_empty() {
            return &self.output;
        }

        let stream_idx = match self
            .streams
            .iter()
            .position(|s| s.media_type == media_type && s.track_index == track_index)
        {
            Some(idx) => idx,
            None => return &self.output,
        };

        let pid = self.streams[stream_idx].pid;
        let pts_90k = ms_to_ts(pts_ms);
        let dts_90k = ms_to_ts(dts_ms);

        // Insert PAT/PMT before keyframes or every ~500ms
        let should_insert_tables = match self.last_pat_pmt_dts {
            None => true,
            Some(last) => is_keyframe || (dts_ms - last).abs() >= 500,
        };

        if should_insert_tables {
            self.write_pat();
            self.write_pmt();
            self.last_pat_pmt_dts = Some(dts_ms);
        }

        // Build PES header on the stack — no allocation, no payload copy.
        // The logical PES is pes_hdr[..hdr_len] ++ payload.
        let pts_differs = pts_90k != dts_90k;
        let mut pes_hdr = [0u8; 19];
        pes_hdr[0..3].copy_from_slice(&PES_START_CODE);
        pes_hdr[3] = match media_type {
            MediaType::Video => 0xE0,
            MediaType::Audio => 0xC0,
        };
        let hdr_len: usize = if pts_differs { 19 } else { 14 };
        let pes_data_len = hdr_len - 6 + payload.len();
        if media_type == MediaType::Audio && pes_data_len <= 0xFFFF {
            pes_hdr[4] = (pes_data_len >> 8) as u8;
            pes_hdr[5] = pes_data_len as u8;
        }
        pes_hdr[6] = 0x80;
        pes_hdr[7] = if pts_differs { 0xC0 } else { 0x80 };
        pes_hdr[8] = if pts_differs { 10 } else { 5 };
        write_timestamp_buf(
            &mut pes_hdr[9..14],
            pts_90k,
            if pts_differs { 0x03 } else { 0x02 },
        );
        if pts_differs {
            write_timestamp_buf(&mut pes_hdr[14..19], dts_90k, 0x01);
        }

        let total_pes = hdr_len + payload.len();
        let ts_count = total_pes.div_ceil(184); // upper bound
        self.output
            .reserve(ts_count * TS_PACKET_SIZE + 2 * TS_PACKET_SIZE);

        // Packetize: walk two logical slices (pes_hdr, payload) without copying
        // them into a contiguous PES buffer.
        let mut pes_offset = 0usize;
        let mut first = true;

        while pes_offset < total_pes {
            let base = self.output.len();
            self.output.resize(base + TS_PACKET_SIZE, 0xFF);
            let ts = &mut self.output[base..base + TS_PACKET_SIZE];

            ts[0] = TS_SYNC_BYTE;
            let pusi_bit: u8 = if first { 0x40 } else { 0x00 };
            ts[1] = pusi_bit | ((pid >> 8) as u8 & 0x1F);
            ts[2] = pid as u8;

            let cc = self.continuity[stream_idx];
            self.continuity[stream_idx] = (cc + 1) & 0x0F;

            let remaining_pes = total_pes - pes_offset;

            let header_end = if first && (is_keyframe || pid == self.pcr_pid) {
                let pcr_bytes = if pid == self.pcr_pid { 6 } else { 0 };
                let af_flags: u8 =
                    if is_keyframe { 0x40 } else { 0x00 } | if pcr_bytes > 0 { 0x10 } else { 0x00 };

                let min_af_len = 1 + pcr_bytes;
                let available = TS_PACKET_SIZE - 4 - 1 - min_af_len;
                let payload_in_packet = remaining_pes.min(available);
                let stuff = available - payload_in_packet;
                let af_len = min_af_len + stuff;

                ts[3] = 0x30 | cc;
                ts[4] = af_len as u8;
                ts[5] = af_flags;

                if pcr_bytes > 0 {
                    write_pcr(&mut ts[6..], dts_90k);
                }

                5 + af_len
            } else {
                let available = TS_PACKET_SIZE - 4;
                let payload_in_packet = remaining_pes.min(available);

                if payload_in_packet < available {
                    let stuff = available - payload_in_packet;
                    if stuff == 1 {
                        ts[3] = 0x30 | cc;
                        ts[4] = 0x00;
                        5
                    } else {
                        ts[3] = 0x30 | cc;
                        ts[4] = (stuff - 1) as u8;
                        if stuff >= 2 {
                            ts[5] = 0x00;
                        }
                        4 + stuff
                    }
                } else {
                    ts[3] = 0x10 | cc;
                    4
                }
            };

            // Copy from the two logical PES slices into this TS packet's
            // payload region, without ever building a contiguous PES buffer.
            let payload_space = TS_PACKET_SIZE - header_end;
            let copy_len = remaining_pes.min(payload_space);
            let dst_start = TS_PACKET_SIZE - copy_len;
            copy_pes_slices(
                &mut ts[dst_start..],
                &pes_hdr[..hdr_len],
                payload,
                pes_offset,
                copy_len,
            );
            pes_offset += copy_len;

            first = false;
        }

        &self.output
    }

    fn write_pat(&mut self) {
        let mut ts = [0xFFu8; TS_PACKET_SIZE];
        ts[0] = TS_SYNC_BYTE;
        ts[1] = 0x40; // PUSI, PID=0
        ts[2] = 0x00;
        ts[3] = 0x10 | (self.pat_cc & 0x0F);
        self.pat_cc = (self.pat_cc + 1) & 0x0F;

        ts[4] = 0x00; // pointer field

        // PAT section
        let pat = &mut ts[5..];
        pat[0] = 0x00; // table_id
        // section_length = 9 (5 header + 4 program entry) — no CRC for simplicity
        // Actually, PAT with CRC: section_length includes from tsid to CRC
        // tsid(2) + version(1) + section(1) + last_section(1) + program(4) + crc(4) = 13
        pat[1] = 0xB0;
        pat[2] = 13;
        pat[3] = 0x00; // transport_stream_id
        pat[4] = 0x01;
        pat[5] = 0xC1; // version=0, current
        pat[6] = 0x00; // section_number
        pat[7] = 0x00; // last_section_number
        // Program 1 → PMT PID
        pat[8] = 0x00;
        pat[9] = 0x01; // program_number = 1
        pat[10] = 0xE0 | ((self.pmt_pid >> 8) as u8 & 0x1F);
        pat[11] = self.pmt_pid as u8;

        let crc = crc32_mpeg2(&ts[5..5 + 12]);
        ts[17] = (crc >> 24) as u8;
        ts[18] = (crc >> 16) as u8;
        ts[19] = (crc >> 8) as u8;
        ts[20] = crc as u8;

        self.output.extend_from_slice(&ts);
    }

    fn write_pmt(&mut self) {
        let mut ts = [0xFFu8; TS_PACKET_SIZE];
        ts[0] = TS_SYNC_BYTE;
        ts[1] = 0x40 | ((self.pmt_pid >> 8) as u8 & 0x1F);
        ts[2] = self.pmt_pid as u8;
        ts[3] = 0x10 | (self.pmt_cc & 0x0F);
        self.pmt_cc = (self.pmt_cc + 1) & 0x0F;

        ts[4] = 0x00; // pointer field

        let pmt = &mut ts[5..];
        pmt[0] = 0x02; // table_id
        // section body: program_number(2) + version(1) + section(1) + last_section(1) + pcr_pid(2) + program_info_length(2) + stream entries + crc(4)
        let entry_size = 5 * self.streams.len();
        let section_len = 9 + entry_size + 4; // 9 fixed + entries + CRC
        pmt[1] = 0xB0 | ((section_len >> 8) as u8 & 0x0F);
        pmt[2] = section_len as u8;
        pmt[3] = 0x00;
        pmt[4] = 0x01; // program_number = 1
        pmt[5] = 0xC1; // version=0, current
        pmt[6] = 0x00;
        pmt[7] = 0x00;
        pmt[8] = 0xE0 | ((self.pcr_pid >> 8) as u8 & 0x1F);
        pmt[9] = self.pcr_pid as u8;
        pmt[10] = 0xF0;
        pmt[11] = 0x00; // program_info_length = 0

        let mut pos = 12;
        for s in &self.streams {
            pmt[pos] = s.stream_type;
            pmt[pos + 1] = 0xE0 | ((s.pid >> 8) as u8 & 0x1F);
            pmt[pos + 2] = s.pid as u8;
            pmt[pos + 3] = 0xF0;
            pmt[pos + 4] = 0x00; // es_info_length = 0
            pos += 5;
        }

        let crc_start = 5;
        let crc_end = 5 + section_len - 4 + 3; // table_id through last entry
        let crc = crc32_mpeg2(&ts[crc_start..crc_end]);
        let crc_pos = crc_end;
        ts[crc_pos] = (crc >> 24) as u8;
        ts[crc_pos + 1] = (crc >> 16) as u8;
        ts[crc_pos + 2] = (crc >> 8) as u8;
        ts[crc_pos + 3] = crc as u8;

        self.output.extend_from_slice(&ts);
    }
}

// --- Timestamp helpers ---

fn parse_timestamp(data: &[u8]) -> i64 {
    let b0 = data[0] as i64;
    let b1 = data[1] as i64;
    let b2 = data[2] as i64;
    let b3 = data[3] as i64;
    let b4 = data[4] as i64;

    ((b0 >> 1) & 0x07) << 30 | (b1 << 22) | ((b2 >> 1) << 15) | (b3 << 7) | (b4 >> 1)
}

#[cfg(test)]
fn write_timestamp(buf: &mut Vec<u8>, ts: i64, marker: u8) {
    buf.push((marker << 4) | (((ts >> 30) as u8) & 0x07) << 1 | 0x01);
    buf.push(((ts >> 22) & 0xFF) as u8);
    buf.push((((ts >> 15) & 0x7F) as u8) << 1 | 0x01);
    buf.push(((ts >> 7) & 0xFF) as u8);
    buf.push((((ts) & 0x7F) as u8) << 1 | 0x01);
}

fn write_timestamp_buf(buf: &mut [u8], ts: i64, marker: u8) {
    buf[0] = (marker << 4) | (((ts >> 30) as u8) & 0x07) << 1 | 0x01;
    buf[1] = ((ts >> 22) & 0xFF) as u8;
    buf[2] = (((ts >> 15) & 0x7F) as u8) << 1 | 0x01;
    buf[3] = ((ts >> 7) & 0xFF) as u8;
    buf[4] = (((ts) & 0x7F) as u8) << 1 | 0x01;
}

fn copy_pes_slices(dst: &mut [u8], hdr: &[u8], payload: &[u8], offset: usize, len: usize) {
    let hdr_len = hdr.len();
    let mut written = 0;
    if offset < hdr_len {
        let from_hdr = (hdr_len - offset).min(len);
        dst[..from_hdr].copy_from_slice(&hdr[offset..offset + from_hdr]);
        written = from_hdr;
    }
    if written < len {
        let payload_offset = offset.saturating_sub(hdr_len);
        let remaining = len - written;
        dst[written..written + remaining]
            .copy_from_slice(&payload[payload_offset..payload_offset + remaining]);
    }
}

fn write_pcr(buf: &mut [u8], ts_90k: i64) {
    let pcr_base = ts_90k.max(0) as u64;
    let pcr_ext: u16 = 0;
    buf[0] = (pcr_base >> 25) as u8;
    buf[1] = (pcr_base >> 17) as u8;
    buf[2] = (pcr_base >> 9) as u8;
    buf[3] = (pcr_base >> 1) as u8;
    buf[4] = ((pcr_base & 1) << 7) as u8 | 0x7E | ((pcr_ext >> 8) as u8 & 0x01);
    buf[5] = pcr_ext as u8;
}

fn ts_to_ms(ts_90k: i64) -> i64 {
    // Exact integer arithmetic: 90kHz → ms is ts / 90 (= ts * 1000 / 90000).
    // Using f64 would introduce up to ~45 ms of accumulated drift over a 24-hour
    // stream because f64 has only 53-bit mantissa precision and ts_90k grows to
    // ~7.8e12 for a day-long stream, losing sub-90-tick resolution.
    ts_90k / 90
}

fn ms_to_ts(ms: i64) -> i64 {
    ms * 90
}

// --- CRC-32/MPEG-2 ---

fn crc32_mpeg2(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = (i as u32) << 24;
            for _ in 0..8 {
                if crc & 0x8000_0000 != 0 {
                    crc = (crc << 1) ^ 0x04C1_1DB7;
                } else {
                    crc <<= 1;
                }
            }
            *entry = crc;
        }
        table
    });

    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let idx = (((crc >> 24) ^ (byte as u32)) & 0xFF) as usize;
        crc = (crc << 8) ^ table[idx];
    }
    crc
}

// --- Codec probing ---

fn probe_video(kind: StreamKind, pes_payload: &[u8]) -> VideoMeta {
    let mut meta = VideoMeta {
        codec: kind.codec_name().to_string(),
        width: 0,
        height: 0,
        fps: 0.0,
        bw: None,
        profile: None,
        level: None,
        pixel_format: None,
    };

    match kind {
        StreamKind::H264 => {
            if let Some(ref sps) = find_h264_sps(pes_payload) {
                parse_h264_sps(sps, &mut meta);
            }
        }
        StreamKind::H265 => {
            if let Some(ref raw_sps) = find_h265_sps(pes_payload) {
                let sps = remove_emulation_prevention(raw_sps);
                parse_h265_sps(&sps, &mut meta);
            }
        }
        _ => {}
    }

    meta
}

fn probe_audio(kind: StreamKind, track_index: u32, pes_payload: &[u8]) -> AudioMeta {
    let mut meta = AudioMeta {
        codec: kind.codec_name().to_string(),
        sample_rate: 0,
        channels: 0,
        channel_layout: None,
        track_index,
        profile: None,
    };

    if kind == StreamKind::AacAdts && pes_payload.len() >= 7 {
        // ADTS header parsing
        if pes_payload[0] == 0xFF && (pes_payload[1] & 0xF0) == 0xF0 {
            let profile_idx = (pes_payload[2] >> 6) as usize;
            meta.profile = match profile_idx {
                0 => Some("Main".to_string()),
                1 => Some("LC".to_string()),
                2 => Some("SSR".to_string()),
                3 => Some("LTP/Reserved".to_string()),
                _ => None,
            };
            let sample_rate_idx = ((pes_payload[2] >> 2) & 0x0F) as usize;
            let channel_config = ((pes_payload[2] & 0x01) << 2) | ((pes_payload[3] >> 6) & 0x03);

            const SAMPLE_RATES: [u32; 13] = [
                96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000,
                7350,
            ];

            if sample_rate_idx < SAMPLE_RATES.len() {
                meta.sample_rate = SAMPLE_RATES[sample_rate_idx];
            }
            meta.channels = channel_config as u32;
            if meta.channels == 7 {
                meta.channels = 8;
            }
        }
    }

    meta
}

// --- H.264 NAL unit scanning ---

fn h264_is_keyframe(payload: &[u8]) -> bool {
    for_each_nal(payload, |nal_type, _nal_data| {
        // NAL type 5 = IDR slice
        if nal_type == 5 {
            return true;
        }
        false
    })
}

fn find_h264_sps(payload: &[u8]) -> Option<Vec<u8>> {
    let mut result = None;
    for_each_nal_raw(payload, |nal_data| {
        if !nal_data.is_empty() && (nal_data[0] & 0x1F) == 7 {
            result = Some(nal_data[1..].to_vec());
            return true;
        }
        false
    });
    result
}

fn parse_h264_sps(raw_sps: &[u8], meta: &mut VideoMeta) {
    if raw_sps.len() < 4 {
        return;
    }

    // Remove emulation prevention bytes (0x00 0x00 0x03 → 0x00 0x00)
    let sps = remove_emulation_prevention(raw_sps);

    let profile_idc = sps[0];
    let level_idc = sps[2];

    meta.profile = Some(
        match profile_idc {
            66 => "Baseline",
            77 => "Main",
            88 => "Extended",
            100 => "High",
            110 => "High 10",
            122 => "High 4:2:2",
            244 => "High 4:4:4 Predictive",
            _ => "Unknown",
        }
        .to_string(),
    );
    meta.level = Some(format!("{}.{}", level_idc / 10, level_idc % 10));

    // Parse SPS via exp-golomb for resolution
    let mut reader = BitReader::new(&sps[3..]);
    let _seq_parameter_set_id = reader.read_ue();

    let chroma_format_idc;
    if profile_idc == 100
        || profile_idc == 110
        || profile_idc == 122
        || profile_idc == 244
        || profile_idc == 44
        || profile_idc == 83
        || profile_idc == 86
        || profile_idc == 118
        || profile_idc == 128
    {
        chroma_format_idc = reader.read_ue();
        if chroma_format_idc == 3 {
            reader.skip(1); // separate_colour_plane_flag
        }
        let _bit_depth_luma = reader.read_ue(); // + 8
        let _bit_depth_chroma = reader.read_ue(); // + 8
        reader.skip(1); // qpprime_y_zero_transform_bypass_flag
        let scaling_matrix_present = reader.read_bits(1);
        if scaling_matrix_present == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for _ in 0..count {
                let present = reader.read_bits(1);
                if present == 1 {
                    let size = if count < 6 { 16 } else { 64 };
                    skip_scaling_list(&mut reader, size);
                }
            }
        }
    } else {
        chroma_format_idc = 1;
    }

    let _log2_max_frame_num = reader.read_ue(); // + 4
    let pic_order_cnt_type = reader.read_ue();
    if pic_order_cnt_type == 0 {
        let _log2_max_pic_order_cnt_lsb = reader.read_ue(); // + 4
    } else if pic_order_cnt_type == 1 {
        reader.skip(1); // delta_pic_order_always_zero_flag
        reader.read_se(); // offset_for_non_ref_pic
        reader.read_se(); // offset_for_top_to_bottom_field
        let num = reader.read_ue();
        for _ in 0..num {
            reader.read_se();
        }
    }

    let _max_num_ref_frames = reader.read_ue();
    reader.skip(1); // gaps_in_frame_num_allowed
    let pic_width = reader.read_ue();
    let pic_height = reader.read_ue();
    let frame_mbs_only = reader.read_bits(1);

    let sub_wc: u32 = if chroma_format_idc == 1 || chroma_format_idc == 2 {
        2
    } else {
        1
    };
    let sub_hc: u32 = if chroma_format_idc == 1 { 2 } else { 1 };

    let mut crop_left = 0u32;
    let mut crop_right = 0u32;
    let mut crop_top = 0u32;
    let mut crop_bottom = 0u32;

    if frame_mbs_only == 0 {
        reader.skip(1); // mb_adaptive_frame_field_flag
    }
    reader.skip(1); // direct_8x8_inference_flag

    let frame_cropping = reader.read_bits(1);
    if frame_cropping == 1 {
        crop_left = reader.read_ue();
        crop_right = reader.read_ue();
        crop_top = reader.read_ue();
        crop_bottom = reader.read_ue();
    }

    let width = (pic_width + 1) * 16 - sub_wc * (crop_left + crop_right);
    let height = (2 - frame_mbs_only) * (pic_height + 1) * 16
        - sub_hc * (2 - frame_mbs_only) * (crop_top + crop_bottom);

    meta.width = width;
    meta.height = height;

    // VUI for frame rate
    let vui_present = reader.read_bits(1);
    if vui_present == 1 {
        let aspect_ratio_present = reader.read_bits(1);
        if aspect_ratio_present == 1 {
            let sar_idx = reader.read_bits(8);
            if sar_idx == 255 {
                reader.skip(32); // sar_width + sar_height
            }
        }
        let overscan_present = reader.read_bits(1);
        if overscan_present == 1 {
            reader.skip(1);
        }
        let video_signal_present = reader.read_bits(1);
        if video_signal_present == 1 {
            reader.skip(3); // video_format
            reader.skip(1); // video_full_range
            let colour_desc_present = reader.read_bits(1);
            if colour_desc_present == 1 {
                reader.skip(24); // primaries + transfer + matrix
            }
        }
        let chroma_loc_present = reader.read_bits(1);
        if chroma_loc_present == 1 {
            reader.read_ue();
            reader.read_ue();
        }
        let timing_info_present = reader.read_bits(1);
        if timing_info_present == 1 {
            let num_units_in_tick = reader.read_bits(32);
            let time_scale = reader.read_bits(32);
            if num_units_in_tick > 0 {
                meta.fps = time_scale as f64 / (2.0 * num_units_in_tick as f64);
            }
        }
    }
}

fn skip_scaling_list(reader: &mut BitReader, size: usize) {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = reader.read_se();
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
}

// --- H.265 NAL unit scanning ---

fn h265_is_keyframe(payload: &[u8]) -> bool {
    for_each_nal_h265(payload, |nal_type, _nal_data| {
        // H.265 IRAP NAL types: BLA_W_LP(16)..RSV_IRAP_VCL23(23)
        (16..=23).contains(&nal_type)
    })
}

fn find_h265_sps(payload: &[u8]) -> Option<Vec<u8>> {
    let mut result = None;
    for_each_nal_raw(payload, |nal_data| {
        if nal_data.len() >= 2 && ((nal_data[0] >> 1) & 0x3F) == 33 {
            result = Some(nal_data[2..].to_vec());
            return true;
        }
        false
    });
    result
}

/// Iterate over Annex B NAL units with H.265 NAL type extraction.
fn for_each_nal_h265<F>(data: &[u8], mut callback: F) -> bool
where
    F: FnMut(u8, &[u8]) -> bool,
{
    for_each_nal_raw(data, |nal_data| {
        if nal_data.is_empty() {
            return false;
        }
        // H.265 NAL header: forbidden(1) + nal_unit_type(6) + nuh_layer_id(6) + nuh_temporal_id_plus1(3)
        let nal_type = (nal_data[0] >> 1) & 0x3F;
        // Skip the 2-byte NAL header for payload
        let payload_start = if nal_data.len() >= 2 {
            2
        } else {
            nal_data.len()
        };
        callback(nal_type, &nal_data[payload_start..])
    })
}

fn parse_h265_sps(sps: &[u8], meta: &mut VideoMeta) {
    if sps.len() < 2 {
        return;
    }

    // H.265 SPS starts after NAL header (2 bytes: forbidden_zero + nal_unit_type + nuh_layer_id + nuh_temporal_id_plus1)
    let mut reader = BitReader::new(sps);
    let _vps_id = reader.read_bits(4);
    let max_sub_layers = reader.read_bits(3) + 1;
    reader.skip(1); // temporal_id_nesting

    // profile_tier_level
    reader.skip(2); // general_profile_space
    reader.skip(1); // general_tier_flag
    let general_profile_idc = reader.read_bits(5);
    reader.skip(32); // general_profile_compatibility_flags
    reader.skip(48); // general_constraint_indicator_flags
    let general_level_idc = reader.read_bits(8);

    meta.profile = Some(
        match general_profile_idc {
            1 => "Main",
            2 => "Main 10",
            3 => "Main Still Picture",
            _ => "Unknown",
        }
        .to_string(),
    );
    meta.level = Some(format!(
        "{}.{}",
        general_level_idc / 30,
        (general_level_idc % 30) / 3
    ));

    // Skip sub-layer profile info
    if max_sub_layers > 1 {
        let mut sub_layer_profile_present = [false; 8];
        let mut sub_layer_level_present = [false; 8];
        for i in 0..(max_sub_layers - 1) as usize {
            sub_layer_profile_present[i] = reader.read_bits(1) == 1;
            sub_layer_level_present[i] = reader.read_bits(1) == 1;
        }
        if max_sub_layers < 8 {
            reader.skip((8 - max_sub_layers) * 2);
        }
        for i in 0..(max_sub_layers - 1) as usize {
            if sub_layer_profile_present[i] {
                reader.skip(88); // profile info
            }
            if sub_layer_level_present[i] {
                reader.skip(8);
            }
        }
    }

    let _sps_id = reader.read_ue();
    let chroma_format_idc = reader.read_ue();
    if chroma_format_idc == 3 {
        reader.skip(1); // separate_colour_plane_flag
    }

    let width = reader.read_ue();
    let height = reader.read_ue();

    let conformance_window = reader.read_bits(1);
    if conformance_window == 1 {
        let sub_wc: u32 = if chroma_format_idc == 1 || chroma_format_idc == 2 {
            2
        } else {
            1
        };
        let sub_hc: u32 = if chroma_format_idc == 1 { 2 } else { 1 };
        let crop_left = reader.read_ue() * sub_wc;
        let crop_right = reader.read_ue() * sub_wc;
        let crop_top = reader.read_ue() * sub_hc;
        let crop_bottom = reader.read_ue() * sub_hc;
        meta.width = width - crop_left - crop_right;
        meta.height = height - crop_top - crop_bottom;
    } else {
        meta.width = width;
        meta.height = height;
    }

    let bit_depth_luma = reader.read_ue() + 8;
    let _bit_depth_chroma = reader.read_ue() + 8;
    let log2_max_pic_order_cnt = reader.read_ue() + 4;

    let sub_layer_ordering_info_present = reader.read_bits(1);
    let start = if sub_layer_ordering_info_present == 1 {
        0
    } else {
        max_sub_layers - 1
    };
    for _ in start..max_sub_layers {
        reader.read_ue(); // max_dec_pic_buffering
        reader.read_ue(); // max_num_reorder_pics
        reader.read_ue(); // max_latency_increase
    }

    let _log2_min_luma_coding_block_size = reader.read_ue() + 3;
    let _log2_diff_max_min_luma_coding_block_size = reader.read_ue();
    let _log2_min_luma_transform_block_size = reader.read_ue() + 2;
    let _log2_diff_max_min_luma_transform_block_size = reader.read_ue();
    let _max_transform_hierarchy_depth_inter = reader.read_ue();
    let _max_transform_hierarchy_depth_intra = reader.read_ue();

    let scaling_list_enabled = reader.read_bits(1);
    if scaling_list_enabled == 1 {
        let scaling_list_data_present = reader.read_bits(1);
        if scaling_list_data_present == 1 {
            return; // too complex to skip reliably
        }
    }

    reader.skip(1); // amp_enabled_flag
    reader.skip(1); // sample_adaptive_offset_enabled_flag

    let pcm_enabled = reader.read_bits(1);
    if pcm_enabled == 1 {
        reader.skip(4); // pcm_sample_bit_depth_luma_minus1
        reader.skip(4); // pcm_sample_bit_depth_chroma_minus1
        reader.read_ue(); // log2_min_pcm_luma_coding_block_size_minus3
        reader.read_ue(); // log2_diff_max_min_pcm_luma_coding_block_size
        reader.skip(1); // pcm_loop_filter_disabled_flag
    }

    let num_short_term_rps = reader.read_ue();
    // Skip short-term RPS — complex variable-length structure
    for i in 0..num_short_term_rps {
        let inter_ref_pic_set = if i > 0 { reader.read_bits(1) } else { 0 };
        if inter_ref_pic_set == 1 {
            if i == num_short_term_rps {
                reader.read_ue(); // delta_idx_minus1
            }
            reader.skip(1); // delta_rps_sign
            reader.read_ue(); // abs_delta_rps_minus1
            return; // inter-prediction RPS is too complex to walk generically
        } else {
            let num_negative = reader.read_ue();
            let num_positive = reader.read_ue();
            for _ in 0..num_negative {
                reader.read_ue(); // delta_poc_s0_minus1
                reader.skip(1); // used_by_curr_pic_s0_flag
            }
            for _ in 0..num_positive {
                reader.read_ue(); // delta_poc_s1_minus1
                reader.skip(1); // used_by_curr_pic_s1_flag
            }
        }
    }

    let long_term_ref_pics_present = reader.read_bits(1);
    if long_term_ref_pics_present == 1 {
        let num_long_term_ref_pics = reader.read_ue();
        for _ in 0..num_long_term_ref_pics {
            reader.read_bits(log2_max_pic_order_cnt); // lt_ref_pic_poc_lsb
            reader.skip(1); // used_by_curr_pic_lt_flag
        }
    }

    reader.skip(1); // sps_temporal_mvp_enabled_flag
    reader.skip(1); // strong_intra_smoothing_enabled_flag

    let vui_present = reader.read_bits(1);
    if vui_present == 1 {
        let aspect_ratio_present = reader.read_bits(1);
        if aspect_ratio_present == 1 {
            let sar_idx = reader.read_bits(8);
            if sar_idx == 255 {
                reader.skip(32); // sar_width + sar_height
            }
        }
        let overscan_present = reader.read_bits(1);
        if overscan_present == 1 {
            reader.skip(1);
        }
        let video_signal_present = reader.read_bits(1);
        if video_signal_present == 1 {
            reader.skip(3 + 1); // video_format + video_full_range
            let colour_desc_present = reader.read_bits(1);
            if colour_desc_present == 1 {
                reader.skip(24); // colour_primaries + transfer + matrix
            }
        }
        let chroma_loc_present = reader.read_bits(1);
        if chroma_loc_present == 1 {
            reader.read_ue();
            reader.read_ue();
        }
        reader.skip(3); // neutral_chroma, field_seq, frame_field_info

        let default_display_window = reader.read_bits(1);
        if default_display_window == 1 {
            reader.read_ue();
            reader.read_ue();
            reader.read_ue();
            reader.read_ue();
        }

        let timing_info_present = reader.read_bits(1);
        if timing_info_present == 1 {
            let num_units_in_tick = reader.read_bits(32);
            let time_scale = reader.read_bits(32);
            if num_units_in_tick > 0 {
                meta.fps = time_scale as f64 / num_units_in_tick as f64;
            }
        }
    }

    let _ = (bit_depth_luma, log2_max_pic_order_cnt);
}

// --- Annex B NAL scanning ---

/// Iterate over Annex B NAL units with H.264 NAL type extraction.
fn for_each_nal<F>(data: &[u8], mut callback: F) -> bool
where
    F: FnMut(u8, &[u8]) -> bool,
{
    for_each_nal_raw(data, |nal_data| {
        if nal_data.is_empty() {
            return false;
        }
        let nal_type = nal_data[0] & 0x1F;
        // Skip the 1-byte NAL header for payload
        callback(nal_type, &nal_data[1..])
    })
}

/// Raw Annex B start-code scanner. Callback receives full NAL data (including header).
fn for_each_nal_raw<F>(data: &[u8], mut callback: F) -> bool
where
    F: FnMut(&[u8]) -> bool,
{
    let starts = crate::media::codec::find_annexb_start_codes(data);
    if starts.is_empty() {
        return false;
    }
    for i in 0..starts.len() {
        let nalu_start = starts[i].1;
        let nalu_end = if i + 1 < starts.len() {
            starts[i + 1].0
        } else {
            data.len()
        };
        if nalu_start < nalu_end && callback(&data[nalu_start..nalu_end]) {
            return true;
        }
    }
    false
}

/// Remove RBSP emulation prevention bytes (0x00 0x00 0x03 → 0x00 0x00).
fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

// --- Bit reader for exp-golomb ---

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn read_bit(&mut self) -> u32 {
        if self.byte_pos >= self.data.len() {
            return 0;
        }
        let bit = (self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        bit as u32
    }

    fn read_bits(&mut self, n: u32) -> u32 {
        let mut val = 0u32;
        for _ in 0..n {
            val = (val << 1) | self.read_bit();
        }
        val
    }

    fn skip(&mut self, n: u32) {
        for _ in 0..n {
            self.read_bit();
        }
    }

    fn read_ue(&mut self) -> u32 {
        let mut leading_zeros = 0u32;
        while self.read_bit() == 0 {
            leading_zeros += 1;
            if leading_zeros > 32 {
                return 0;
            }
        }
        if leading_zeros == 0 {
            return 0;
        }
        let val = self.read_bits(leading_zeros);
        (1 << leading_zeros) - 1 + val
    }

    fn read_se(&mut self) -> i32 {
        let ue = self.read_ue();
        if ue.is_multiple_of(2) {
            -(ue as i32 / 2)
        } else {
            (ue as i32 + 1) / 2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timestamp_round_trip() {
        let ts: i64 = 132000; // 90kHz timestamp
        let mut buf = Vec::new();
        write_timestamp(&mut buf, ts, 0x02);
        let parsed = parse_timestamp(&buf);
        assert_eq!(parsed, ts);
    }

    #[test]
    fn parse_timestamp_large_value() {
        let ts: i64 = 8_589_934_591; // max 33-bit value
        let mut buf = Vec::new();
        write_timestamp(&mut buf, ts, 0x03);
        let parsed = parse_timestamp(&buf);
        assert_eq!(parsed, ts);
    }

    #[test]
    fn ts_ms_conversion() {
        assert_eq!(ts_to_ms(90000), 1000);
        assert_eq!(ts_to_ms(0), 0);
        assert_eq!(ms_to_ts(1000), 90000);
        assert_eq!(ms_to_ts(0), 0);
    }

    #[test]
    fn ts_to_ms_no_float_drift() {
        // Verify no floating-point drift at 24-hour scale.
        // At 90 kHz, 24 hours = 24*3600*90000 = 7_776_000_000 ticks.
        // f64 has 53-bit mantissa; at this scale each ULP is ~1024 ticks = ~11 ms.
        // Integer division: ts / 90 must give exact ms with no drift.
        let day_90k: i64 = 24 * 3600 * 90_000;
        let day_ms: i64 = 24 * 3600 * 1000;
        assert_eq!(
            ts_to_ms(day_90k),
            day_ms,
            "ts_to_ms must be exact for 24-hour timestamps (no f64 drift)"
        );
        // Also verify round-trip for a one-hour mark
        let hour_90k: i64 = 3600 * 90_000;
        let hour_ms: i64 = 3600 * 1000;
        assert_eq!(ts_to_ms(hour_90k), hour_ms);
    }

    #[test]
    fn crc32_known_value() {
        // PAT with known CRC
        let data = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE1, 0x00,
        ];
        let crc = crc32_mpeg2(&data);
        assert_ne!(crc, 0); // Just verify it produces a non-trivial value
        // The expected CRC32/MPEG-2 of this PAT payload is 0xE8F95E7D
        assert_eq!(crc, 0xE8F95E7D);
    }

    #[test]
    fn crc32_bit_at_a_time_equivalence() {
        // Local reference implementation of the bit-at-a-time algorithm
        let reference_crc = |data: &[u8]| {
            let mut crc = 0xFFFF_FFFFu32;
            for &byte in data {
                crc ^= (byte as u32) << 24;
                for _ in 0..8 {
                    if crc & 0x8000_0000 != 0 {
                        crc = (crc << 1) ^ 0x04C1_1DB7;
                    } else {
                        crc <<= 1;
                    }
                }
            }
            crc
        };

        // Test with different sizes and randomized inputs
        let mut rng = 12345u32;
        let mut next_random_byte = || {
            // simple LCG generator
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            (rng >> 24) as u8
        };

        for size in [0, 1, 4, 12, 188, 1024, 4096] {
            for _ in 0..10 {
                let data: Vec<u8> = (0..size).map(|_| next_random_byte()).collect();
                let ref_val = reference_crc(&data);
                let table_val = crc32_mpeg2(&data);
                assert_eq!(
                    table_val, ref_val,
                    "Failed equivalence test at size {}",
                    size
                );
            }
        }
    }

    #[test]
    fn demux_fixture_file() {
        let ts_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/artifacts/latest/correctness-h264.ts"
        ));
        let ts_data = match ts_data {
            Ok(d) => d,
            Err(_) => return, // Skip if fixture not available
        };

        let mut demuxer = TsDemuxer::new();
        demuxer.feed(&ts_data);
        demuxer.flush();
        let packets = demuxer.drain();

        assert!(!packets.is_empty(), "should produce packets");

        let video_count = packets
            .iter()
            .filter(|p| p.media_type == MediaType::Video)
            .count();
        let audio_count = packets
            .iter()
            .filter(|p| p.media_type == MediaType::Audio)
            .count();
        assert!(video_count > 0, "should have video packets");
        assert!(audio_count > 0, "should have audio packets");

        // Check that at least one keyframe exists
        let keyframes = packets.iter().filter(|p| p.is_keyframe).count();
        assert!(keyframes > 0, "should have keyframes");

        // PTS should be monotonically non-decreasing per stream
        let mut last_video_pts = i64::MIN;
        let mut last_audio_pts = i64::MIN;
        for pkt in &packets {
            match pkt.media_type {
                MediaType::Video => {
                    // DTS must be non-decreasing (PTS can jump with B-frames)
                    // Just verify PTS is reasonable (positive)
                    assert!(
                        pkt.pts >= 0,
                        "video PTS should be non-negative: {}",
                        pkt.pts
                    );
                    last_video_pts = pkt.pts;
                }
                MediaType::Audio => {
                    assert!(
                        pkt.pts >= last_audio_pts,
                        "audio PTS should be non-decreasing: {} < {}",
                        pkt.pts,
                        last_audio_pts
                    );
                    last_audio_pts = pkt.pts;
                }
            }
        }
        let _ = last_video_pts;

        // Check probe
        let mut demuxer2 = TsDemuxer::new();
        demuxer2.feed(&ts_data);
        demuxer2.flush();
        let probe = demuxer2.take_probe();
        assert!(probe.is_some(), "should produce probe result");
        let probe = probe.unwrap();

        if let Some(ref video) = probe.video {
            assert_eq!(video.codec, "h264");
            assert_eq!(video.width, 1920);
            assert_eq!(video.height, 1080);
            assert!(
                (video.fps - 30.0).abs() < 1.0,
                "fps should be ~30: {}",
                video.fps
            );
            assert_eq!(video.profile.as_deref(), Some("High"));
        } else {
            panic!("should probe video metadata");
        }

        assert!(!probe.audio_tracks.is_empty());
        assert_eq!(probe.audio_tracks[0].codec, "aac");
        assert_eq!(probe.audio_tracks[0].sample_rate, 48000);
    }

    #[test]
    fn drain_into_reuses_output_batches() {
        let mut demuxer = TsDemuxer::new();
        let mut output = Vec::with_capacity(4);

        assert_eq!(demuxer.drain_into(&mut output), 0);
        assert!(output.is_empty());
        assert!(output.capacity() >= 4);
    }

    #[test]
    fn demux_chunked_feed() {
        let ts_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/artifacts/latest/correctness-h264.ts"
        ));
        let ts_data = match ts_data {
            Ok(d) => d,
            Err(_) => return,
        };

        // Feed in 1316-byte chunks (SRT packet size)
        let mut demuxer = TsDemuxer::new();
        for chunk in ts_data.chunks(1316) {
            demuxer.feed(chunk);
        }
        demuxer.flush();
        let packets = demuxer.drain();

        assert!(!packets.is_empty(), "chunked feed should produce packets");

        // Feed all at once for comparison
        let mut demuxer2 = TsDemuxer::new();
        demuxer2.feed(&ts_data);
        demuxer2.flush();
        let packets2 = demuxer2.drain();

        assert_eq!(
            packets.len(),
            packets2.len(),
            "chunked and full feed should produce same packet count"
        );
    }

    #[test]
    fn demux_h265_fixture_file() {
        let ts_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/artifacts/latest/correctness-h265.ts"
        ));
        let ts_data = match ts_data {
            Ok(d) => d,
            Err(_) => return,
        };

        let mut demuxer = TsDemuxer::new();
        for chunk in ts_data.chunks(1316) {
            demuxer.feed(chunk);
        }
        demuxer.flush();
        let packets = demuxer.drain();

        assert!(!packets.is_empty(), "should produce packets");

        let video_count = packets
            .iter()
            .filter(|p| p.media_type == MediaType::Video)
            .count();
        let audio_count = packets
            .iter()
            .filter(|p| p.media_type == MediaType::Audio)
            .count();
        assert!(video_count > 0, "should have video packets");
        assert!(audio_count > 0, "should have audio packets");

        let keyframes = packets.iter().filter(|p| p.is_keyframe).count();
        assert!(keyframes > 0, "should have keyframes");

        let mut demuxer2 = TsDemuxer::new();
        demuxer2.feed(&ts_data);
        demuxer2.flush();
        let probe = demuxer2.take_probe();
        assert!(probe.is_some(), "should produce probe result");
        let probe = probe.unwrap();

        if let Some(ref video) = probe.video {
            assert_eq!(video.codec, "hevc");
            assert_eq!(video.width, 1920);
            assert_eq!(video.height, 1080);
            assert!(
                (video.fps - 30.0).abs() < 1.0,
                "fps should be ~30: {}",
                video.fps
            );
            assert_eq!(video.profile.as_deref(), Some("Main"));
        } else {
            panic!("should probe video metadata");
        }

        assert!(!probe.audio_tracks.is_empty());
        assert_eq!(probe.audio_tracks[0].codec, "aac");
        assert_eq!(probe.audio_tracks[0].sample_rate, 48000);
    }

    #[test]
    fn demux_corrupt_input_no_panic() {
        let mut demuxer = TsDemuxer::new();

        // Empty input
        demuxer.feed(&[]);
        assert!(demuxer.drain().is_empty());

        // Random garbage
        demuxer.feed(&[0xDE, 0xAD, 0xBE, 0xEF, 0x47, 0x00]);
        assert!(demuxer.drain().is_empty());

        // Truncated TS packet
        let short = vec![0x47u8; 100];
        demuxer.feed(&short);
        assert!(demuxer.drain().is_empty());

        // All zeros
        demuxer.feed(&[0u8; 188]);
        assert!(demuxer.drain().is_empty());
    }

    #[test]
    fn mux_round_trip() {
        let video = VideoMeta {
            codec: "h264".to_string(),
            width: 640,
            height: 360,
            fps: 30.0,
            bw: None,
            profile: Some("High".to_string()),
            level: Some("3.0".to_string()),
            pixel_format: None,
        };

        let audio = AudioMeta {
            codec: "aac".to_string(),
            sample_rate: 48000,
            channels: 2,
            channel_layout: None,
            track_index: 0,
            profile: None,
        };

        let mut muxer = TsMuxer::new(Some(&video), &[audio]);

        // Create test packets
        let video_payload = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC]; // IDR NAL
        let audio_payload = vec![0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0xDE, 0x02]; // ADTS

        let ts_out1 = muxer.mux_packet(MediaType::Video, 0, 0, 0, true, &video_payload);
        assert!(!ts_out1.is_empty());
        assert_eq!(ts_out1.len() % TS_PACKET_SIZE, 0);

        let ts_out2 = muxer.mux_packet(MediaType::Audio, 0, 0, 0, false, &audio_payload);
        assert!(!ts_out2.is_empty());
        assert_eq!(ts_out2.len() % TS_PACKET_SIZE, 0);
    }

    #[test]
    fn mux_demux_round_trip() {
        let video = VideoMeta {
            codec: "h264".to_string(),
            width: 320,
            height: 240,
            fps: 30.0,
            bw: None,
            profile: None,
            level: None,
            pixel_format: None,
        };

        let audio = AudioMeta {
            codec: "aac".to_string(),
            sample_rate: 44100,
            channels: 2,
            channel_layout: None,
            track_index: 0,
            profile: None,
        };

        let mut muxer = TsMuxer::new(Some(&video), &[audio]);
        let mut all_ts = Vec::new();

        // Mux a few packets
        let video_payload = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84];
        let audio_payload = vec![0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC];

        for i in 0..5 {
            let pts = i * 33; // ~30fps
            let ts = muxer.mux_packet(MediaType::Video, 0, pts, pts, i == 0, &video_payload);
            all_ts.extend_from_slice(ts);

            let ts = muxer.mux_packet(MediaType::Audio, 0, pts, pts, false, &audio_payload);
            all_ts.extend_from_slice(ts);
        }

        // Demux it back
        let mut demuxer = TsDemuxer::new();
        demuxer.feed(&all_ts);
        demuxer.flush();
        let packets = demuxer.drain();

        let video_count = packets
            .iter()
            .filter(|p| p.media_type == MediaType::Video)
            .count();
        let audio_count = packets
            .iter()
            .filter(|p| p.media_type == MediaType::Audio)
            .count();

        assert_eq!(video_count, 5, "round-trip should preserve video count");
        assert_eq!(audio_count, 5, "round-trip should preserve audio count");

        // First video should be keyframe
        let first_video = packets
            .iter()
            .find(|p| p.media_type == MediaType::Video)
            .unwrap();
        assert!(first_video.is_keyframe, "first video should be keyframe");
    }

    #[test]
    fn nal_scanner_h264_idr() {
        // Start code + IDR NAL
        let data = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB];
        assert!(h264_is_keyframe(&data));

        // Start code + non-IDR slice
        let data2 = [0x00, 0x00, 0x00, 0x01, 0x41, 0xAA, 0xBB];
        assert!(!h264_is_keyframe(&data2));
    }

    #[test]
    fn h265_irap_detection() {
        // H.265 NAL header: byte0 = forbidden(1b) | nal_unit_type(6b) >> ... encoded as (type << 1)
        // IDR_W_RADL = type 19 → byte0 = (19 << 1) = 0x26, byte1 = 0x01 (layer=0, tid=1)
        // for_each_nal_h265 extracts: (byte0 >> 1) & 0x3F = (0x26 >> 1) & 0x3F = 19 ✓
        let idr_nal = vec![0x00, 0x00, 0x00, 0x01, 0x26u8, 0x01, 0xAA, 0xBB];
        assert!(
            h265_is_keyframe(&idr_nal),
            "IDR_W_RADL (type 19) should be a keyframe"
        );

        // IDR_N_LP = type 20 → byte0 = (20 << 1) = 0x28
        let idr_nlp = vec![0x00, 0x00, 0x00, 0x01, 0x28u8, 0x01, 0xCC];
        assert!(
            h265_is_keyframe(&idr_nlp),
            "IDR_N_LP (type 20) should be a keyframe"
        );

        // Non-IRAP: TRAIL_R = type 1 → byte0 = (1 << 1) = 0x02
        let trail_r = vec![0x00, 0x00, 0x00, 0x01, 0x02u8, 0x01, 0xDD];
        assert!(
            !h265_is_keyframe(&trail_r),
            "TRAIL_R (type 1) should not be a keyframe"
        );

        // CRA_NUT = type 21 → byte0 = (21 << 1) = 0x2A
        // CRA is commonly produced by software encoders (ffmpeg, x265) and hardware
        // encoders. Must be treated as a keyframe for ring-buffer overflow recovery.
        let cra = vec![0x00, 0x00, 0x00, 0x01, 0x2Au8, 0x01, 0xEE];
        assert!(
            h265_is_keyframe(&cra),
            "CRA_NUT (type 21) should be a keyframe"
        );

        // BLA_W_LP = type 16 → byte0 = (16 << 1) = 0x20 (low boundary of IRAP range)
        let bla = vec![0x00, 0x00, 0x00, 0x01, 0x20u8, 0x01, 0xFF];
        assert!(
            h265_is_keyframe(&bla),
            "BLA_W_LP (type 16) should be a keyframe"
        );

        // Type 15 (non-IRAP, just below boundary) → byte0 = (15 << 1) = 0x1E
        let non_irap_below = vec![0x00, 0x00, 0x00, 0x01, 0x1Eu8, 0x01, 0x00];
        assert!(
            !h265_is_keyframe(&non_irap_below),
            "Type 15 is non-IRAP, should not be a keyframe"
        );

        // Type 24 (just above IRAP range) → byte0 = (24 << 1) = 0x30
        let non_irap_above = vec![0x00, 0x00, 0x00, 0x01, 0x30u8, 0x01, 0x00];
        assert!(
            !h265_is_keyframe(&non_irap_above),
            "Type 24 is non-IRAP, should not be a keyframe"
        );
    }

    #[test]
    fn pat_pmt_parsing() {
        // Build a minimal PAT + PMT
        let mut ts_data = Vec::new();

        // PAT packet
        let mut pat_pkt = [0xFFu8; 188];
        pat_pkt[0] = 0x47;
        pat_pkt[1] = 0x40; // PUSI, PID=0
        pat_pkt[2] = 0x00;
        pat_pkt[3] = 0x10; // payload only, CC=0
        pat_pkt[4] = 0x00; // pointer
        pat_pkt[5] = 0x00; // table_id = PAT
        pat_pkt[6] = 0xB0;
        pat_pkt[7] = 13; // section_length
        pat_pkt[8] = 0x00;
        pat_pkt[9] = 0x01; // TSID
        pat_pkt[10] = 0xC1; // version
        pat_pkt[11] = 0x00;
        pat_pkt[12] = 0x00;
        // Program 1 → PMT PID 0x1000
        pat_pkt[13] = 0x00;
        pat_pkt[14] = 0x01;
        pat_pkt[15] = 0xF0;
        pat_pkt[16] = 0x00;
        let crc = crc32_mpeg2(&pat_pkt[5..17]);
        pat_pkt[17] = (crc >> 24) as u8;
        pat_pkt[18] = (crc >> 16) as u8;
        pat_pkt[19] = (crc >> 8) as u8;
        pat_pkt[20] = crc as u8;
        ts_data.extend_from_slice(&pat_pkt);

        // PMT packet (1 video + 1 audio)
        let mut pmt_pkt = [0xFFu8; 188];
        pmt_pkt[0] = 0x47;
        pmt_pkt[1] = 0x50; // PUSI, PID=0x1000
        pmt_pkt[2] = 0x00;
        pmt_pkt[3] = 0x10;
        pmt_pkt[4] = 0x00;
        pmt_pkt[5] = 0x02; // table_id = PMT
        let section_len = 9 + 10 + 4; // 9 fixed + 2 streams — 5 + CRC
        pmt_pkt[6] = 0xB0;
        pmt_pkt[7] = section_len as u8;
        pmt_pkt[8] = 0x00;
        pmt_pkt[9] = 0x01;
        pmt_pkt[10] = 0xC1;
        pmt_pkt[11] = 0x00;
        pmt_pkt[12] = 0x00;
        pmt_pkt[13] = 0xE1;
        pmt_pkt[14] = 0x00; // PCR PID = 0x100
        pmt_pkt[15] = 0xF0;
        pmt_pkt[16] = 0x00; // program_info_length = 0
        // Video: H.264, PID=0x100
        pmt_pkt[17] = 0x1B;
        pmt_pkt[18] = 0xE1;
        pmt_pkt[19] = 0x00;
        pmt_pkt[20] = 0xF0;
        pmt_pkt[21] = 0x00;
        // Audio: AAC, PID=0x101
        pmt_pkt[22] = 0x0F;
        pmt_pkt[23] = 0xE1;
        pmt_pkt[24] = 0x01;
        pmt_pkt[25] = 0xF0;
        pmt_pkt[26] = 0x00;
        let crc2 = crc32_mpeg2(&pmt_pkt[5..27]);
        pmt_pkt[27] = (crc2 >> 24) as u8;
        pmt_pkt[28] = (crc2 >> 16) as u8;
        pmt_pkt[29] = (crc2 >> 8) as u8;
        pmt_pkt[30] = crc2 as u8;
        ts_data.extend_from_slice(&pmt_pkt);

        let mut demuxer = TsDemuxer::new();
        demuxer.feed(&ts_data);

        assert!(demuxer.has_streams());
        assert_eq!(demuxer.streams.len(), 2);
        assert_eq!(demuxer.streams[0].kind, StreamKind::H264);
        assert_eq!(demuxer.streams[0]._pid, 0x100);
        assert_eq!(demuxer.streams[1].kind, StreamKind::AacAdts);
        assert_eq!(demuxer.streams[1]._pid, 0x101);
    }

    #[test]
    fn adts_probe() {
        // Valid ADTS header: 48kHz, mono
        let adts = [0xFF, 0xF1, 0x4C, 0x40, 0x02, 0x1F, 0xFC];
        let meta = probe_audio(StreamKind::AacAdts, 0, &adts);
        assert_eq!(meta.sample_rate, 48000);
        assert_eq!(meta.channels, 1);
    }

    // --- Helpers shared by PMT version tests ---

    /// Build a 188-byte TS PAT packet pointing to PMT PID 0x1000.
    fn make_pat_ts_pkt() -> Vec<u8> {
        let mut pkt = vec![0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40; // PUSI, PID=0x0000
        pkt[2] = 0x00;
        pkt[3] = 0x10; // payload-only, CC=0
        pkt[4] = 0x00; // pointer_field
        // PAT section
        pkt[5] = 0x00; // table_id = PAT
        pkt[6] = 0xB0;
        pkt[7] = 13; // section_length
        pkt[8] = 0x00;
        pkt[9] = 0x01; // TSID = 1
        pkt[10] = 0xC1; // version=0, current_next=1
        pkt[11] = 0x00; // section_number
        pkt[12] = 0x00; // last_section_number
        // Program 1 -> PMT PID 0x1000
        pkt[13] = 0x00;
        pkt[14] = 0x01;
        pkt[15] = 0xF0; // 0xE0 | (0x1000 >> 8) = 0xF0
        pkt[16] = 0x00; // 0x1000 & 0xFF
        let crc = crc32_mpeg2(&pkt[5..17]);
        pkt[17] = (crc >> 24) as u8;
        pkt[18] = (crc >> 16) as u8;
        pkt[19] = (crc >> 8) as u8;
        pkt[20] = crc as u8;
        pkt
    }

    /// Build a 188-byte TS PMT packet at PID 0x1000 with the given version and
    /// stream list. Each stream is `(stream_type, elementary_pid)`.
    fn make_pmt_ts_pkt(version: u8, streams: &[(u8, u16)]) -> Vec<u8> {
        let mut pkt = vec![0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x50; // PUSI, PID high (0x1000)
        pkt[2] = 0x00; // PID low
        pkt[3] = 0x10; // payload-only, CC=0
        pkt[4] = 0x00; // pointer_field
        // PMT section
        pkt[5] = 0x02; // table_id = PMT
        let section_len = 9 + (5 * streams.len()) + 4;
        pkt[6] = 0xB0;
        pkt[7] = section_len as u8;
        pkt[8] = 0x00; // program_number high
        pkt[9] = 0x01; // program_number low
        // version_number in bits 5..1, current_next_indicator in bit 0
        pkt[10] = 0xC0 | ((version & 0x1F) << 1) | 0x01;
        pkt[11] = 0x00; // section_number
        pkt[12] = 0x00; // last_section_number
        pkt[13] = 0xE1; // PCR_PID = 0x100
        pkt[14] = 0x00;
        pkt[15] = 0xF0; // program_info_length high
        pkt[16] = 0x00; // program_info_length low (= 0)
        let mut pos = 17usize;
        for &(stream_type, pid) in streams {
            pkt[pos] = stream_type;
            pkt[pos + 1] = 0xE0 | ((pid >> 8) as u8 & 0x1F);
            pkt[pos + 2] = (pid & 0xFF) as u8;
            pkt[pos + 3] = 0xF0; // ES_info_length = 0
            pkt[pos + 4] = 0x00;
            pos += 5;
        }
        let crc = crc32_mpeg2(&pkt[5..pos]);
        pkt[pos] = (crc >> 24) as u8;
        pkt[pos + 1] = (crc >> 16) as u8;
        pkt[pos + 2] = (crc >> 8) as u8;
        pkt[pos + 3] = crc as u8;
        pkt
    }

    // --- Regression: issue #3 — PMT version tracking ---

    #[test]
    fn pmt_retransmission_same_version_is_idempotent() {
        // Regression: the old guard `if !self.streams.is_empty() && pmt_expected == 0 { return }`
        // was replaced by explicit version tracking. A retransmission of the same
        // PMT version must NOT rebuild the stream map (no phantom duplicates).
        let mut data = Vec::new();
        data.extend_from_slice(&make_pat_ts_pkt());
        let streams = [(0x1B, 0x100u16), (0x0F, 0x101u16)]; // H.264 + AAC
        data.extend_from_slice(&make_pmt_ts_pkt(0, &streams));

        let mut demuxer = TsDemuxer::new();
        demuxer.feed(&data);

        assert_eq!(
            demuxer.streams.len(),
            2,
            "initial PMT must produce 2 streams"
        );
        assert_eq!(demuxer.pmt_version, Some(0));

        // Feed the same PMT version again (broadcaster retransmits every ~100ms)
        let retransmit = make_pmt_ts_pkt(0, &streams);
        demuxer.feed(&retransmit);

        assert_eq!(
            demuxer.streams.len(),
            2,
            "retransmitting the same PMT version must not rebuild the stream map"
        );
        assert_eq!(demuxer.pmt_version, Some(0));
    }

    #[test]
    fn pmt_version_change_rebuilds_stream_map() {
        // Regression: the old code returned early on non-empty streams, silently
        // dropping genuine PMT version changes (e.g., broadcaster adds audio mid-stream).
        let mut data = Vec::new();
        data.extend_from_slice(&make_pat_ts_pkt());
        // Version 0: video only
        data.extend_from_slice(&make_pmt_ts_pkt(0, &[(0x1B, 0x100u16)]));

        let mut demuxer = TsDemuxer::new();
        demuxer.feed(&data);

        assert_eq!(demuxer.streams.len(), 1, "PMT v0 must have 1 stream");
        assert_eq!(demuxer.pmt_version, Some(0));

        // Version 1: broadcaster added an audio track
        let v1_pkt = make_pmt_ts_pkt(1, &[(0x1B, 0x100u16), (0x0F, 0x101u16)]);
        demuxer.feed(&v1_pkt);

        assert_eq!(
            demuxer.streams.len(),
            2,
            "PMT version change must rebuild stream map so new audio PID is parsed"
        );
        assert_eq!(demuxer.pmt_version, Some(1));
        assert_eq!(demuxer.streams[1].kind, StreamKind::AacAdts);
    }

    // --- Regression: issue #12 — PCR negative guard ---

    #[test]
    fn write_pcr_clamps_negative_ts_to_zero() {
        // Regression: before the .max(0) fix, a negative DTS reaching write_pcr
        // would silently cast to a large u64, producing a nonsensical PCR value
        // that makes decoders stall or seek unexpectedly.
        let mut buf_neg = [0u8; 6];
        let mut buf_zero = [0u8; 6];
        write_pcr(&mut buf_neg, -1_000_000);
        write_pcr(&mut buf_zero, 0);
        assert_eq!(
            buf_neg, buf_zero,
            "negative ts_90k must clamp to 0, not wrap to a huge u64 PCR"
        );

        // Also verify an extreme negative value does not panic.
        let mut buf_min = [0u8; 6];
        write_pcr(&mut buf_min, i64::MIN);
        assert_eq!(buf_min, buf_zero);
    }

    // --- Regression: issue #6 (Round 3) — TsDemuxer remainder length cap ---
    // Before the MAX_REMAINDER guard, feeding a stream of single-byte 0x47
    // chunks would cause remainder to grow by 1 byte on every call — O(n)
    // memory growth per byte of input, i.e. O(n²) overall processing cost
    // for a corrupt / adversarial stream.  After the fix the remainder must
    // never exceed TS_PACKET_SIZE - 1 = 187 bytes.
    #[test]
    fn feed_remainder_capped_on_corrupt_stream() {
        let mut dem = TsDemuxer::new();
        // Feed 500 isolated 0x47 bytes (each looks like a TS sync byte but is
        // never followed by 187 more bytes, so no packet can complete).
        for _ in 0..500 {
            dem.feed(&[0x47]);
        }
        assert!(
            dem.remainder.len() < TS_PACKET_SIZE,
            "remainder must be capped at TS_PACKET_SIZE-1 ({}) but was {}",
            TS_PACKET_SIZE - 1,
            dem.remainder.len()
        );
    }

    // --- Regression: issue #5 (Round 4) — PMT version rebuild preserves in-flight PES ---
    // Before the fix, a PMT version change discarded ALL StreamInfo (including
    // PesAccumulator buffers).  A partially-assembled video frame would be lost,
    // producing a glitch until the next IDR.  After the fix, PES buffers for PIDs
    // that survive into the new PMT are carried over.
    #[test]
    fn pmt_version_change_preserves_pes_for_unchanged_pid() {
        // Build a minimal TS PES packet for PID 0x100 that starts a new PES unit
        // (PUSI=1) but does NOT complete it (no second packet with the next PES
        // start, so the frame stays in the accumulator).
        fn make_pes_ts_pkt(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
            let mut pkt = vec![0xFFu8; 188];
            pkt[0] = 0x47;
            pkt[1] = if pusi { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1F);
            pkt[2] = (pid & 0xFF) as u8;
            pkt[3] = 0x10; // payload_unit_start = pusi flag handled above; CC=0
            // When PUSI, prepend a minimal PES header: start code + stream_id +
            // length(0=unbounded) + flags + header_data_length(0)
            let pes_header: &[u8] = if pusi {
                &[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00]
            } else {
                &[]
            };
            let data_offset = 4usize;
            let total = pes_header.len() + payload.len();
            pkt[data_offset..data_offset + total.min(184)]
                .iter_mut()
                .zip(pes_header.iter().chain(payload.iter()))
                .for_each(|(d, &s)| *d = s);
            pkt
        }

        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(&make_pat_ts_pkt());
        // PMT v0: H.264 video at PID 0x100
        data.extend_from_slice(&make_pmt_ts_pkt(0, &[(0x1B, 0x100u16)]));
        // PES start for PID 0x100 — frame not yet complete
        data.extend_from_slice(&make_pes_ts_pkt(0x100, true, &[0xDE, 0xAD]));

        let mut demuxer = TsDemuxer::new();
        demuxer.feed(&data);

        // Verify the PES buffer has data
        assert_eq!(demuxer.streams.len(), 1);
        let buf_before = demuxer.streams[0].pes.buf.clone();
        assert!(
            !buf_before.is_empty(),
            "PES buf must have partial frame data"
        );

        // Now the broadcaster sends a PMT version change for the same PID
        // (e.g., only the language descriptor changed).
        let v1_pkt = make_pmt_ts_pkt(1, &[(0x1B, 0x100u16)]);
        demuxer.feed(&v1_pkt);

        assert_eq!(
            demuxer.streams.len(),
            1,
            "stream map should still have 1 stream"
        );
        assert_eq!(demuxer.pmt_version, Some(1));
        assert_eq!(
            demuxer.streams[0].pes.buf, buf_before,
            "in-flight PES buffer must be preserved after PMT version change for same PID"
        );
    }

    #[test]
    fn crc32_empty_data() {
        assert_eq!(crc32_mpeg2(&[]), 0xFFFF_FFFF);
    }

    #[test]
    fn crc32_known_vector() {
        // CRC-32/MPEG-2 of "123456789" (classic check value)
        let data = b"123456789";
        assert_eq!(crc32_mpeg2(data), 0x0376_E6E7);
    }

    #[test]
    fn crc32_idempotent_across_calls() {
        let data = b"hello world";
        let a = crc32_mpeg2(data);
        let b = crc32_mpeg2(data);
        assert_eq!(a, b);
    }

    #[test]
    fn write_pcr_zero_ts() {
        let mut buf = [0xFFu8; 6];
        write_pcr(&mut buf, 0);
        // PCR at zero: base=0, extension=0, with the PCR marker bits set
        assert_eq!(buf[0], 0x00);
        assert_eq!(buf[1], 0x00);
        assert_eq!(buf[2], 0x00);
        assert_eq!(buf[3], 0x00);
    }
}
