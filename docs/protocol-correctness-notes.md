# Protocol Correctness Notes

This note tracks protocol-level issues that must be correct before treating the
Rust media path as production-ready.

## Probe With The Matching Ingest Protocol

Probing must use the same read protocol as the active ingest protocol:

- RTMP ingest should be probed with RTMP play/read.
- SRT ingest should be probed with SRT read/play.
- File ingest currently normalizes through the local RTMP bridge, so it should be
  treated as RTMP-shaped until the file path moves fully in-process.

Cross-protocol probing can create false positives. For example, probing an SRT
ingest through RTMP requires an additional RTMP packetization layer. If that
layer is incomplete, `ffprobe` can report bogus streams even when the source
media itself is valid. Matching the ingest protocol keeps diagnostics focused on
the source path rather than a different protocol's packaging behavior.

The diagnostics endpoint rejects a requested probe protocol that differs from
the active ingest protocol. It also refuses to start a media probe when there is
no active ingest.

## SRT Stream ID Normalization

The SRT listener must accept the stream-id shapes seen in the supplied pcaps and
common tooling:

```text
publish:live/<key>
publisher:<key>
read:live/<key>
play:<key>
subscriber:<key>
<key>
#!::r=live/<key>,m=publish
#!::r=live/<key>,m=request
```

It must also strip query parameters before database validation:

```text
publish:live/<key>?latency=240000
#!::r=live/<key>,m=publish,latency=240000
```

The query parameters configure transport behavior. They are not part of the
stream key.

## Supplied Capture Coverage

The supplied captures exercise these protocol behaviors:

| Capture | Relevant behavior |
| --- | --- |
| `srt_abl_vmix.pcap` | Multiple SRT publishers, MPEG-TS H.264/AAC, and B-frame timestamp ordering |
| `srt_abl_vmix_default.pcap` | The same publisher family with default and query-suffixed stream IDs |
| `srt.pcap` | SRT handshake, control traffic, retransmission, and MPEG-TS media payload |
| `tvu.pcap` | H.264/AAC media plus an unsupported third track that must be excluded |
| `srt_rtmp.pcap` | SRT ingest followed by RTMP read/packaging |
| `srs-srt-rtmp.pcap` | SRS SRT-to-RTMP comparison traffic |
| `bpf_mod.pcap` | SRT transport/control packet capture behavior |

SRT ACK, NAK, ACKACK, keepalive, handshake, and shutdown packets terminate in
libsrt and are never written to the FFmpeg input queue. Only bytes returned by
`srt_recv` after transport processing reach the MPEG-TS demuxer. The demuxer
then admits the selected video stream and audio streams only; subtitle, data,
attachment, and unknown stream types are excluded from the media ring.

## Media Streams Only

Read endpoints must emit media payload only:

- RTMP play should send RTMP control/session messages through the RTMP session
  serializer and media through RTMP audio/video messages.
- SRT read should emit MPEG-TS bytes only.
- SRT control packets, RTMP status responses, and application metadata must not
  be mixed into MPEG-TS payload bytes.

When validating with `ffprobe`, the expected stream set for the current use case
is exactly one video stream plus the intended audio streams. Extra subtitle,
private-data, or unknown streams should be treated as a correctness bug unless
they are explicitly present in the source and intentionally passed through.

The current pipeline contract selects the first video stream and preserves all
audio tracks. Additional video streams, subtitles, private data, and unknown
stream types are deliberately excluded. Packets from a second video PID must
never be merged into the selected video stream.

The MPEG-TS remuxer must also reject unsupported codec metadata. It must not
guess H.264 for an unknown video codec or AAC for an unknown audio codec,
because a guessed stream declaration can make arbitrary bytes appear as a
spurious stream.

## Timestamp Semantics

RTMP video timestamps are decode timestamps. AVC/HEVC video packets additionally
carry a signed 24-bit composition-time offset:

```text
DTS = RTMP timestamp
PTS = DTS + signed composition-time offset
```

Discarding this offset makes B-frames use the wrong presentation order and can
cause A/V sync drift or non-monotonic timestamp behavior during remuxing. Audio
does not use the FLV video composition offset.

Current defect: ingest stores the two values correctly, but RTMP play and RTMP
egress call `send_video_data` / `publish_video_data` with `packet.pts` as the
RTMP timestamp while forwarding the original FLV payload, which still contains
the composition-time offset. The outbound RTMP timestamp must use
`packet.dts`; otherwise B-frame composition offset can be applied twice.

## H.265

H.265 must be tested explicitly. It is not enough for H.264 to work:

- SRT/MPEG-TS H.265 should preserve HEVC codec identity through demux and read
  remux paths.
- RTMP H.265 requires Enhanced RTMP/HEVC packet handling and cannot be assumed
  to behave like AVC sequence headers.

Until the RTMP H.265 path is proven end-to-end, diagnostics should prefer SRT
read/probe for SRT H.265 publishers.
