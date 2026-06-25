# Configuration Reference

The Rust runtime has a small environment configuration surface for deployment
paths, listener ports, and operational tuning. User-facing settings are stored
in SQLite.

## Fixed Runtime Values and Environment Variables

| Value | Default setting | Environment Variable Override |
|---|---|---|
| Dashboard/API listener | `0.0.0.0:3030` | `RESTREAM_HTTP_PORT` (port only) |
| RTMP listener | `0.0.0.0:1935` | `RESTREAM_RTMP_PORT` |
| SRT listener | `0.0.0.0:10080` | `RESTREAM_SRT_PORT` |
| Transcoder backend | External FFmpeg subprocess | `RESTREAM_USE_INTERNAL_TRANSCODER` (`1`/`true`/`yes`/`on` to enable in-process backend) |
| SQLite database | `data.db` | `RESTREAM_DB_PATH` |
| Media directory | `media/` | `RESTREAM_MEDIA_DIR` |
| File-ingest executable | `ffmpeg` from `PATH` | *None* |
| File descriptor limit | `65536` | `RESTREAM_NOFILE_LIMIT` |
| Output reconciliation interval | 1 second | `RESTREAM_RECONCILE_INTERVAL_MS` |
| Failed-output max retries | `10` | `RESTREAM_OUTPUT_MAX_RETRIES` |
| Failed-output restart base backoff | 5 seconds | `RESTREAM_OUTPUT_RETRY_BASE_MS` |
| Failed-output restart max backoff | 300 seconds | `RESTREAM_OUTPUT_RETRY_MAX_MS` |
| Idle HLS segmenter timeout | 60 seconds | `RESTREAM_HLS_IDLE_TIMEOUT_MS` |
| HLS minimum segment length | 1 second | `RESTREAM_HLS_MIN_SEGMENT_MS` |
| HLS live window length | 20 segments | `RESTREAM_HLS_MAX_SEGMENTS` |
| HLS segment accumulator capacity | 8 MiB | `RESTREAM_HLS_SEGMENT_CAPACITY_BYTES` |

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
| Custom encoding | Stored through `/encodings/custom` for future use; not offered as an output encoding and rejected by output create/update |
| Recording enabled | Stored per pipeline as `recording_enabled:<pipelineId>` |

Sessions are persisted in SQLite and reloaded at startup. Expired sessions are
pruned during initialization and then once per hour while the server is running
(reconciler tick 3600).

## SQLite Performance Settings

The following PRAGMAs are applied at startup after WAL mode is enabled:

| PRAGMA | Value | Effect |
|---|---|---|
| `synchronous` | `NORMAL` | fsync only at WAL checkpoints; safe with WAL |
| `busy_timeout` | 5000 ms | Retry on locked database before returning SQLITE_BUSY |
| `journal_size_limit` | 64 MiB | Caps WAL file growth; excess is reclaimed at checkpoint |
| `cache_size` | -16384 (16 MiB) | Page cache kept in process memory |
| `temp_store` | `MEMORY` | Temporary tables and indices use memory, not disk |
| `mmap_size` | 128 MiB | Read pages via memory-mapped I/O on supported platforms |

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
| `rtmp://...` | Native RTMP egress; IPv6 addresses in bracket notation (`[::1]`) are supported |
| `rtmps://...` | Native RTMPS egress through the RTMP path with TLS before handshake |
| `srt://...` | Native SRT MPEG-TS egress; percent-encoded characters in the `streamid` query parameter are decoded automatically |
| `hls://...` | Starts the pipeline's local in-memory HLS segmenter |
| `http://...`, `https://...` | Starts the local segmenter and uploads segments/playlist with HTTP PUT |

Any other prefix is rejected during validation. For HTTP/HTTPS HLS upload,
segment upload URLs are derived from the playlist target: a `file=` query
parameter is replaced with `seg<N>.ts`, otherwise the playlist path filename is
replaced with the segment filename.

Encoding strings are compound values:

```text
<video-preset>[+<audio-routing>]
```

Examples:

```text
source
720p
1080p+atrack:0
720p+remap:0:1
source+downmix:1
```

Built-in video profiles are `source`, `720p`, `1080p`, and the internal `h264`
conversion profile. `source` is passthrough and bypasses the video transcoder.
For non-source built-in video profiles, the default backend is an external
FFmpeg subprocess that performs decode/scale/encode. When
`RESTREAM_USE_INTERNAL_TRANSCODER=1`, the in-process backend performs
decode/scale/encode for built-in video profiles (audio streams are copied).
`custom` remains stored configuration only. It is rejected by output
create/update so operators do not accidentally select a passthrough path that
looks like custom FFmpeg execution.

Audio routing accepts `atrack`, `remap`, and `downmix`. `atrack` stays on the
packet-only selector path; channel-level `remap` and `downmix` routes run
through an external FFmpeg audio stage and re-encode stereo AAC.

## SRT Socket Policy

Both SRT play (subscriber) and SRT egress connections wait up to 200 ms per
poll for the ingest probe to complete before creating the MPEG-TS muxer.
If no video metadata is available the server polls every 200 ms; if the ingest
disappears during the wait the connection is closed gracefully.

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

The older `/preview/hls/...` paths are compatibility aliases. Live generation
uses the native inline `TsMuxer`; one shared segmenter per pipeline serves
browser previews and HLS-type outputs. The segmenter is kept alive while at
least one persistent HLS output is active; its reference count is adjusted
correctly even when an HLS egress task panics (refcount is decremented in
an always-runs cleanup path outside the panic-catching closure).

These routes require the dashboard session cookie. They still respond with
HLS CORS headers, but unauthenticated playlist and segment requests return
`401`.

Before exposing HLS outside authenticated dashboard sessions, add signed URLs
or short-lived bearer tokens covering both playlists and segments, plus expiry,
revocation, rate limits, cache policy, and token-safe audit logs.
