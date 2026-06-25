//! Shared packet feeder primitives for TS-producing stages.
//!
//! Recording, HLS, and transcoder stdin stages all perform the same packet
//! work: convert payloads into TS-ready elementary stream bytes, map media
//! packets to muxer stream indexes, enforce monotonic DTS, and append MPEG-TS
//! packets to a sink. Keeping that logic here gives stage code a smaller
//! surface area: read bursts, feed packets, flush bytes.

use std::sync::Arc;

use crate::media::codec::{audio_for_ts_into, video_for_ts_into};
use crate::media::engine::{AudioMeta, VideoMeta};
use crate::media::mpegts::TsMuxer;
use crate::media::ring_buffer::{DtsEnforcer, MediaPacket, MediaType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedAction {
    Continue,
    Stop,
}

pub trait FeedSink {
    fn on_ts_bytes(&mut self, bytes: &[u8]) -> FeedAction;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackPolicy {
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedWriteMode {
    Batch,
}

#[derive(Debug, Clone)]
pub struct PacketFeedConfig {
    pub track_policy: TrackPolicy,
    pub write_mode: FeedWriteMode,
    pub video_sequence_header: Option<Vec<u8>>,
}

impl Default for PacketFeedConfig {
    fn default() -> Self {
        Self {
            track_policy: TrackPolicy::All,
            write_mode: FeedWriteMode::Batch,
            video_sequence_header: None,
        }
    }
}

pub struct TsPacketFeeder {
    muxer: TsMuxer,
    dts_enforcer: DtsEnforcer,
    audio_tracks: Arc<Vec<AudioMeta>>,
    has_video: bool,
    nalu_len_size: usize,
    sps_pps_cache: Vec<u8>,
    video_conv_buf: Vec<u8>,
    audio_conv_buf: Vec<u8>,
}

impl TsPacketFeeder {
    pub fn new(
        video: Option<&VideoMeta>,
        audio_tracks: Arc<Vec<AudioMeta>>,
        config: PacketFeedConfig,
    ) -> Self {
        let (nalu_len_size, sps_pps_cache) = config
            .video_sequence_header
            .as_deref()
            .map(parse_video_sequence_header)
            .unwrap_or((4, Vec::new()));
        let num_streams = video.is_some() as usize + audio_tracks.len();

        Self {
            muxer: TsMuxer::new(video, &audio_tracks),
            dts_enforcer: DtsEnforcer::new(num_streams),
            audio_tracks,
            has_video: video.is_some(),
            nalu_len_size,
            sps_pps_cache,
            video_conv_buf: Vec::new(),
            audio_conv_buf: Vec::new(),
        }
    }

    pub fn audio_tracks(&self) -> &Arc<Vec<AudioMeta>> {
        &self.audio_tracks
    }

    pub fn extend_ts_for_packet(&mut self, packet: &MediaPacket, output: &mut Vec<u8>) -> bool {
        let payload = match packet.media_type {
            MediaType::Video => match video_for_ts_into(
                &packet.payload,
                packet.format,
                &mut self.nalu_len_size,
                &mut self.sps_pps_cache,
                &mut self.video_conv_buf,
            ) {
                Some(payload) => payload,
                None => return false,
            },
            MediaType::Audio => {
                let track = self
                    .audio_tracks
                    .iter()
                    .find(|a| a.track_index == packet.track_index)
                    .or(self.audio_tracks.first());
                let (sample_rate, channels) = track
                    .map(|a| (a.sample_rate, a.channels))
                    .unwrap_or((48_000, 1));
                match audio_for_ts_into(
                    &packet.payload,
                    packet.format,
                    sample_rate,
                    channels,
                    &mut self.audio_conv_buf,
                ) {
                    Some(payload) => payload,
                    None => return false,
                }
            }
        };

        let stream_idx = match packet.media_type {
            MediaType::Video => 0,
            MediaType::Audio => {
                let video_offset = self.has_video as usize;
                match self
                    .audio_tracks
                    .iter()
                    .position(|a| a.track_index == packet.track_index)
                {
                    Some(index) => index + video_offset,
                    None => return false,
                }
            }
        };

        let (pts, dts) = self
            .dts_enforcer
            .enforce(stream_idx, packet.pts, packet.dts);
        let ts_bytes = self.muxer.mux_packet(
            packet.media_type,
            packet.track_index,
            pts,
            dts,
            packet.is_keyframe,
            payload,
        );
        if ts_bytes.is_empty() {
            return false;
        }
        output.extend_from_slice(ts_bytes);
        true
    }
}

fn parse_video_sequence_header(flv_sequence_header: &[u8]) -> (usize, Vec<u8>) {
    if flv_sequence_header.len() > 5 {
        crate::media::codec::parse_avcc_config(&flv_sequence_header[5..])
    } else {
        (4, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn audio_track(index: u32) -> AudioMeta {
        AudioMeta {
            codec: "aac".to_string(),
            sample_rate: 48_000,
            channels: 2,
            channel_layout: None,
            track_index: index,
            profile: None,
        }
    }

    fn video_meta() -> VideoMeta {
        VideoMeta {
            codec: "h264".to_string(),
            width: 1920,
            height: 1080,
            fps: 30.0,
            bw: None,
            profile: None,
            level: None,
            pixel_format: None,
        }
    }

    #[test]
    fn feeder_skips_unknown_audio_track_to_protect_dts_state() {
        let audio_tracks = Arc::new(vec![audio_track(0)]);
        let mut feeder = TsPacketFeeder::new(None, audio_tracks, PacketFeedConfig::default());
        let packet = MediaPacket {
            media_type: MediaType::Audio,
            format: crate::media::ring_buffer::PayloadFormat::Raw,
            is_keyframe: false,
            track_index: 7,
            pts: 0,
            dts: 0,
            payload: Bytes::from_static(&[0x00]),
        };
        let mut output = Vec::new();

        assert!(!feeder.extend_ts_for_packet(&packet, &mut output));
        assert!(output.is_empty());
    }

    #[test]
    fn feeder_matches_manual_codec_mux_and_dts_path() {
        use crate::media::codec::{audio_for_ts_into, video_for_ts_into};
        use crate::media::mpegts::TsMuxer;
        use crate::media::ring_buffer::{DtsEnforcer, PayloadFormat};

        let video = video_meta();
        let audio_tracks = Arc::new(vec![audio_track(0), audio_track(1)]);
        let packets = vec![
            MediaPacket {
                media_type: MediaType::Video,
                format: PayloadFormat::Raw,
                is_keyframe: true,
                track_index: 0,
                pts: 0,
                dts: 0,
                payload: Bytes::from_static(&[
                    0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21, 0xA0,
                ]),
            },
            MediaPacket {
                media_type: MediaType::Audio,
                format: PayloadFormat::Raw,
                is_keyframe: false,
                track_index: 0,
                pts: 20,
                dts: 20,
                payload: Bytes::from_static(&[0x11; 32]),
            },
            MediaPacket {
                media_type: MediaType::Audio,
                format: PayloadFormat::Raw,
                is_keyframe: false,
                track_index: 1,
                pts: 21,
                dts: 21,
                payload: Bytes::from_static(&[0x22; 24]),
            },
        ];

        let mut feeder = TsPacketFeeder::new(
            Some(&video),
            audio_tracks.clone(),
            PacketFeedConfig::default(),
        );
        let mut feeder_output = Vec::new();
        for packet in &packets {
            assert!(feeder.extend_ts_for_packet(packet, &mut feeder_output));
        }

        let mut muxer = TsMuxer::new(Some(&video), &audio_tracks);
        let mut dts = DtsEnforcer::new(1 + audio_tracks.len());
        let mut video_conv_buf = Vec::new();
        let mut audio_conv_buf = Vec::new();
        let mut nalu_len_size = 4usize;
        let mut sps_pps_cache = Vec::new();
        let mut manual_output = Vec::new();

        for packet in &packets {
            let payload = match packet.media_type {
                MediaType::Video => video_for_ts_into(
                    &packet.payload,
                    packet.format,
                    &mut nalu_len_size,
                    &mut sps_pps_cache,
                    &mut video_conv_buf,
                )
                .expect("video packet should convert"),
                MediaType::Audio => {
                    let track = audio_tracks
                        .iter()
                        .find(|a| a.track_index == packet.track_index)
                        .expect("test packet track exists");
                    audio_for_ts_into(
                        &packet.payload,
                        packet.format,
                        track.sample_rate,
                        track.channels,
                        &mut audio_conv_buf,
                    )
                    .expect("audio packet should convert")
                }
            };
            let stream_idx = match packet.media_type {
                MediaType::Video => 0,
                MediaType::Audio => {
                    1 + audio_tracks
                        .iter()
                        .position(|a| a.track_index == packet.track_index)
                        .expect("test packet track exists")
                }
            };
            let (pts, dts_value) = dts.enforce(stream_idx, packet.pts, packet.dts);
            manual_output.extend_from_slice(muxer.mux_packet(
                packet.media_type,
                packet.track_index,
                pts,
                dts_value,
                packet.is_keyframe,
                payload,
            ));
        }

        assert_eq!(feeder_output, manual_output);
    }
}
