# Health Mapping

`GET /health` is built on demand from native `MediaEngine` state and per-pipeline
recording settings in SQLite. It does not call MediaMTX, ffprobe, or child
FFmpeg progress endpoints.

## Top-Level Shape

```json
{
  "generatedAt": "2026-06-20T12:00:00Z",
  "status": "ready",
  "pipelines": {},
  "srtListener": {
    "bondingAvailable": false,
    "udpRxQueueBytes": 0,
    "udpRxQueuePeakBytes": 0,
    "udpDrops": 0
  }
}
```

`status` is currently always `ready` when the handler returns. `/healthz` is the
separate process-liveness endpoint and returns `{ "status": "ok" }`.

## Input Status

For every configured pipeline:

| Condition | `input.status` |
|---|---|
| `MediaEngine::active_ingests` contains the pipeline ID | `on` |
| No active ingest is registered | `off` |

The Rust implementation does not currently emit `warning` or `error` for input
state. The frontend may still understand those legacy values, but `/health`
does not derive them.

Active input fields:

| Field | Source |
|---|---|
| `publishStartedAt` | Current UTC time minus the ingest's monotonic uptime |
| `bytesReceived` | Ingest `AtomicU64` counter |
| `bitrateKbps` | Average bytes received over total ingest uptime |
| `video` | RTMP FLV parser or SRT native TsDemuxer metadata |
| `audio` | Primary audio metadata |
| `publisher.protocol` | `rtmp`, `srt`, or `file` |
| `publisher.remoteAddr` | Accepted peer address when available |
| `publisher.quality` | Protocol-specific live transport snapshot |

`bytesSent`, `readers`, and `unexpectedReaders.count` are currently emitted as
zero placeholders.

### RTMP Quality

On Linux, the accepted socket is queried with `TCP_INFO` and `SO_MEMINFO` about
every two seconds. Fields include RTT, receive RTT, bytes received, last-receive
age, receive space/window, out-of-order packets, receive-buffer occupancy, and a
rate derived from consecutive byte samples.

The first rate sample is unavailable because no prior counter exists. On
unsupported hosts or collection failure, `tcpStatsUnavailableReason` explains
the absence.

### SRT Quality

The receive loop samples `srt_bistats()` approximately once per second.
Cumulative loss/drop/retransmit/undecrypt counters are retained for context.
Alerting should use the per-second delta fields so a recovered connection can
return to healthy.

The snapshot also includes SRT buffer occupancy and packets in flight when
libsrt provides them. For SRT publishers it also reports:

| Field | Meaning |
|---|---|
| `srtBonded` | Whether libsrt accepted this publisher as a socket group |
| `srtGroupMemberCount` | Total member tuples currently reported for the group |
| `srtGroupConnectedMembers` | Members in the connected state |
| `srtGroupActiveMembers` | Members carrying the active backup-group path |
| `srtGroupBrokenMembers` | Members reported broken |

The member-count fields are omitted for ordinary single-link publishers.

## Output Status

Active native egresses appear in `pipelines[id].outputs`:

- `register_egress()` stores `active_egresses[outputId]` with an explicit
  `pipeline_id`;
- `health_snapshot()` includes entries whose `ActiveEgress.pipeline_id`
  matches the pipeline being rendered.

| Field | Source |
|---|---|
| `status` | `ActiveEgress.status` (normally `running`) |
| `totalSize` | Atomic bytes-sent counter |
| `bitrateKbps` | Byte delta divided by elapsed sample time; cached between samples |
| `startedAt` | Egress registration timestamp |

Stopped configured outputs are absent rather than being emitted with `off`.
The dashboard merges those definitions from `/config`; active output counters
come from `/health`.

The egress bitrate updates only after a sample window longer than 0.5 seconds
and only when the byte counter advances.

## Recording State

Each pipeline includes:

```json
{
  "recording": {
    "enabled": true,
    "active": true
  }
}
```

- `enabled` comes from SQLite key `recording_enabled:<pipelineId>`.
- `active` reflects a live recording cancellation token.

The reconciler starts an enabled recording when an ingest is active and stops
it when the ingest disappears or recording is disabled.

## SRT Listener State

The shared SRT listener monitor reads Linux `/proc/net/udp` and tracks:

- whether the linked libsrt accepted `SRTO_GROUPCONNECT` at startup
- current receive-queue bytes
- peak receive-queue bytes since process start
- cumulative kernel UDP drops

These are listener-wide values, not per-pipeline values. A false
`bondingAvailable` value means ordinary SRT works but the installed libsrt must
be rebuilt with `ENABLE_BONDING=ON` before bonded ingest can work. The static
release build supplies a bonding-enabled libsrt; development builds still
report the capability of whichever system library they link.
