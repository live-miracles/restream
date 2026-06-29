//! Domain-level audio-routing grammar shared by planner and media backends.
//!
//! These types describe what an encoding string means for audio selection or
//! transformation. Backend modules can then decide how to execute the routing
//! without owning the grammar themselves.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioRouting {
    /// Pass all audio streams through unchanged.
    Passthrough,
    /// Select specific audio tracks by 0-based index.
    SelectTracks(Vec<usize>),
    /// Remap stereo channels: (left_channel, right_channel, optional_track).
    Remap {
        left: usize,
        right: usize,
        track: usize,
    },
    /// Downmix a specific audio track to stereo.
    Downmix(usize),
}

pub fn parse_audio_routing(encoding: &str) -> AudioRouting {
    let audio_part = if let Some(pos) = encoding.find('+') {
        &encoding[pos + 1..]
    } else if encoding.starts_with("remap:")
        || encoding.starts_with("atrack:")
        || encoding.starts_with("downmix:")
    {
        encoding
    } else {
        return AudioRouting::Passthrough;
    };

    if let Some(rest) = audio_part.strip_prefix("remap:") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() >= 2 {
            let left = parts[0].parse().unwrap_or(0);
            let right = parts[1].parse().unwrap_or(1);
            let track = parts.get(2).and_then(|t| t.parse().ok()).unwrap_or(0);
            return AudioRouting::Remap { left, right, track };
        }
    } else if let Some(rest) = audio_part.strip_prefix("atrack:") {
        let tracks: Vec<usize> = rest.split(',').filter_map(|t| t.parse().ok()).collect();
        if !tracks.is_empty() {
            return AudioRouting::SelectTracks(tracks);
        }
    } else if let Some(rest) = audio_part.strip_prefix("downmix:")
        && let Ok(track) = rest.parse()
    {
        return AudioRouting::Downmix(track);
    }

    AudioRouting::Passthrough
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_passthrough_for_plain_video_preset() {
        assert!(matches!(
            parse_audio_routing("720p"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("source"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("1080p"),
            AudioRouting::Passthrough
        ));
    }

    #[test]
    fn routing_select_tracks_single() {
        let routing = parse_audio_routing("720p+atrack:0");
        assert!(matches!(routing, AudioRouting::SelectTracks(ref t) if t == &[0]));
    }

    #[test]
    fn routing_select_tracks_multiple() {
        let routing = parse_audio_routing("source+atrack:0,2,5");
        assert!(matches!(routing, AudioRouting::SelectTracks(ref t) if t == &[0, 2, 5]));
    }

    #[test]
    fn routing_select_tracks_invalid_falls_back_to_passthrough() {
        assert!(matches!(
            parse_audio_routing("720p+atrack:abc"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("720p+atrack:"),
            AudioRouting::Passthrough
        ));
    }

    #[test]
    fn routing_remap_two_channel() {
        let routing = parse_audio_routing("720p+remap:0:1");
        assert!(matches!(
            routing,
            AudioRouting::Remap {
                left: 0,
                right: 1,
                track: 0
            }
        ));
    }

    #[test]
    fn routing_remap_with_track_index() {
        let routing = parse_audio_routing("source+remap:0:1:3");
        assert!(matches!(
            routing,
            AudioRouting::Remap {
                left: 0,
                right: 1,
                track: 3
            }
        ));
    }

    #[test]
    fn routing_remap_default_fallback() {
        let routing = parse_audio_routing("720p+remap:0");
        assert!(matches!(routing, AudioRouting::Passthrough));
    }

    #[test]
    fn routing_downmix_single_track() {
        let routing = parse_audio_routing("source+downmix:0");
        assert!(matches!(routing, AudioRouting::Downmix(0)));
        let routing = parse_audio_routing("720p+downmix:3");
        assert!(matches!(routing, AudioRouting::Downmix(3)));
    }

    #[test]
    fn routing_downmix_invalid_falls_back_to_passthrough() {
        assert!(matches!(
            parse_audio_routing("720p+downmix:abc"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("720p+downmix:"),
            AudioRouting::Passthrough
        ));
    }

    #[test]
    fn routing_atrack_standalone() {
        let routing = parse_audio_routing("atrack:0,1");
        assert!(matches!(routing, AudioRouting::SelectTracks(ref t) if t == &[0, 1]));
    }

    #[test]
    fn routing_remap_standalone() {
        let routing = parse_audio_routing("remap:0:1");
        assert!(matches!(
            routing,
            AudioRouting::Remap {
                left: 0,
                right: 1,
                track: 0
            }
        ));
    }

    #[test]
    fn routing_downmix_standalone() {
        let routing = parse_audio_routing("downmix:0");
        assert!(matches!(routing, AudioRouting::Downmix(0)));
    }

    #[test]
    fn parse_passthrough() {
        assert!(matches!(
            parse_audio_routing("source"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("720p"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(parse_audio_routing(""), AudioRouting::Passthrough));
    }

    #[test]
    fn parse_atrack() {
        match parse_audio_routing("720p+atrack:0,1") {
            AudioRouting::SelectTracks(t) => assert_eq!(t, vec![0, 1]),
            other => panic!("expected SelectTracks, got {:?}", other),
        }
        match parse_audio_routing("source+atrack:2") {
            AudioRouting::SelectTracks(t) => assert_eq!(t, vec![2]),
            other => panic!("expected SelectTracks, got {:?}", other),
        }
    }

    #[test]
    fn parse_remap() {
        match parse_audio_routing("source+remap:0:1") {
            AudioRouting::Remap { left, right, track } => {
                assert_eq!((left, right, track), (0, 1, 0));
            }
            other => panic!("expected Remap, got {:?}", other),
        }
        match parse_audio_routing("720p+remap:1:0:2") {
            AudioRouting::Remap { left, right, track } => {
                assert_eq!((left, right, track), (1, 0, 2));
            }
            other => panic!("expected Remap, got {:?}", other),
        }
    }

    #[test]
    fn parse_downmix() {
        match parse_audio_routing("source+downmix:1") {
            AudioRouting::Downmix(t) => assert_eq!(t, 1),
            other => panic!("expected Downmix, got {:?}", other),
        }
    }

    #[test]
    fn parse_legacy_remap() {
        match parse_audio_routing("remap:0:1") {
            AudioRouting::Remap { left, right, track } => {
                assert_eq!((left, right, track), (0, 1, 0));
            }
            other => panic!("expected Remap, got {:?}", other),
        }
    }
}
