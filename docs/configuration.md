# Configuration Reference

The Rust runtime currently has a small configuration surface. Listener ports can be overridden via environment variables; runtime paths are fixed in code; user-facing settings are stored in SQLite.

## Fixed Runtime Values and Environment Variables

| Value | Default setting | Environment Variable Override |
|---|---|---|
| Dashboard/API listener | `0.0.0.0:3030` | `RESTREAM_HTTP_PORT` (port only) |
| RTMP listener | `0.0.0.0:1935` | `RESTREAM_RTMP_PORT` |
| SRT listener | `0.0.0.0:10080` | `RESTREAM_SRT_PORT` |
| SQLite database | `data.db` | *None* |
| Media directory | `media/` | *None* |
| File-ingest executable | `ffmpeg` from `PATH` | *None* |
| Output reconciliation interval | 1 second | *None* |
| Failed-output restart delay | 5 seconds | *None* |

The Rust server does not currently read the old Node environment variables such
as `BASE_PATH`, `PUBLIC_INGEST_HOST`, `HEALTH_SNAPSHOT_INTERVAL_MS`,
or the old output-recovery knobs. Do not depend on those variables.

## SQLite-Backed Settings

`GET /config` returns the current values. `PATCH /config` updates any supplied
field.

```json
{
  "serverName": "Name",
  "ingestHost": "stream.example.com",
  "ingestSecurity": {
    "failureLimit": 10,
    "failureWindowMs": 60000,
    "banMs": 600000,
    "trackedIpLimit": 10000
  }
}
```

| Setting | Behavior |
|---|---|
| `serverName` | Dashboard display name; must be non-empty |
| `ingestHost` | Hostname used when generating RTMP/SRT publisher URLs; blank falls back to `localhost` |
| `ingestSecurity` | In-memory failed-publish tracking and temporary IP bans; changes are persisted |
| Dashboard password | Scrypt hash stored in SQLite; first-run password is `admin` |
| Custom encoding | Stored through `/encodings/custom`; not yet applied by the in-process transcoder |
| Recording enabled | Stored per pipeline as `recording_enabled:<pipelineId>` |

Sessions are persisted in SQLite and reloaded at startup. Expired sessions are
pruned during initialization.

## Ingest URLs

Generated publisher URLs use the configured ingest host and fixed native ports:

```text
rtmp://<ingestHost>:1935/live/<streamKey>
srt://<ingestHost>:10080?streamid=publish:live/<streamKey>
```

The application exposes 20 built-in stream keys through `GET /stream-keys`.
Pipelines select one of those keys; the API does not currently create or delete
stream-key records independently.

## Output Configuration

Each output stores:

```json
{
  "name": "Primary CDN",
  "url": "rtmp://destination.example/live/key",
  "encoding": "source"
}
```

Supported routing behavior:

| URL | Runtime behavior |
|---|---|
| `rtmp://...` | Native RTMP egress |
| `srt://...` | Native SRT MPEG-TS egress |
| `hls://...`, `http://...`, `https://...` | Starts the pipeline's local in-memory HLS segmenter |

Any other prefix is rejected during validation. The target URL for HLS routing is not used for HTTP HLS upload; the target host is ignored after the protocol decision.

Encoding strings are compound values:

```text
<video-preset>[+<audio-routing>]
```

Examples:

```text
source
720p
1080p+atrack:0
2160p+remap:0:1
source+downmix:1
```

Recognized video presets are `source`, `720p`, `1080p`, `2160p`/`4k`,
`vertical-crop`, `vertical-rotate`, and the internal `h264` conversion preset.
Only source passthrough is currently trustworthy: the transcoder configures
encoder/output metadata but does not decode, filter, and encode the compressed
packets. `custom` is accepted but also behaves as passthrough.

Audio routing parsers accept `atrack`, `remap`, and `downmix`. Only stream-level
selection is complete; channel filtering/encoding remains open.

## SRT Socket Policy

The runtime calls its high-bitrate helper for the SRT listener and single-link
egress sockets:

- 250 ms latency
- 256-packet loss/reorder tolerance
- 8 MiB UDP send/receive buffers
- 12 MiB SRT send/receive buffers
- 32768-packet flow-control window
- unlimited automatic maximum bandwidth

The code does not explicitly apply the helper to accepted sockets or bonded
egress groups. Do not assume those sockets have every requested value without
runtime verification.

Linux startup checks warn when `net.core.rmem_max` or `net.core.wmem_max` cannot
support the requested UDP buffers. The listener's `/proc/net/udp` receive queue
and drop count are exported in `/health`.

SRT egress backup links can be supplied with:

```text
srt://primary.example:10080?streamid=publish:live/key&bond=backup1.example:10080,backup2.example:10080
```

This code path is unit-tested for URL parsing and socket-option constants, but
still needs live multi-link interoperability validation.

Inbound bonding uses the same single listener. When the publisher initiates a
real SRT group, `srt_accept` returns one group ID and libsrt attaches later
links in the background. Merely opening two independent sockets with the same
StreamID does not create a bond.

The linked libsrt must be built with `ENABLE_BONDING=ON`. The listener checks
`SRTO_GROUPCONNECT` at startup and logs a warning when a distribution library
provides only disabled group stubs; ordinary single-link SRT remains enabled.
The static release setup builds pinned SRT 1.5.5 with bonding enabled and runs
separate-process broadcast and backup/failover tests before packaging. This
does not require a second ingest endpoint: all member tuples join the group
accepted from the shared listener.

## HLS Pull and Authorization

The in-memory HLS store is served at:

```text
/hls/<pipelineId>
/hls/<pipelineId>/index.m3u8
/hls/<pipelineId>/seg<N>.ts
```

The older `/preview/hls/...` paths are compatibility aliases. Store/route tests
pass, but live generation is not yet considered protocol-correct because the
feeder concatenates raw `MediaPacket.payload` values into FFmpeg format
detection rather than supplying a defined container stream.

These routes are currently unauthenticated. Before exposing them publicly, add
signed URLs or short-lived bearer tokens covering both playlists and segments,
plus expiry, revocation, rate limits, cache policy, and token-safe audit logs.
