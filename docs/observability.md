# Observability

The Rust rewrite exposes JSON and SSE diagnostics directly from process state.
It does not currently expose a Prometheus text endpoint or proxy Grafana.

## Available Surfaces

| Surface | Authentication | Purpose |
|---|---|---|
| `GET /healthz` | None | Process liveness |
| `GET /health` | None | Pipeline input/output state, transport quality, recording state, SRT listener pressure |
| `GET /metrics/system` | Session | CPU, memory, disk, and host-wide network rates |
| `GET /api/status` | Session | Restream version/commit, linked FFmpeg version, OS/host information |
| `GET /pipelines/:id/probe` | Session | Active input codec, dimensions, audio tracks, bitrate, and GOP summary |
| `GET /pipelines/:id/graph` | Session | Processing stages, buffers, and output connections |
| `GET /pipelines/:id/diagnostics` | Session | Nine-check diagnostic run streamed over SSE |

`/health` is assembled on demand from `MediaEngine`; there is no MediaMTX poll
or background health-snapshot cache.

## Publisher Transport Metrics

SRT publisher quality is sampled with `srt_bistats()` and includes:

- RTT, receive rate, and estimated link capacity
- receive latency/buffer values and NAK count
- cumulative loss, drop, retransmit, and undecrypt counters
- per-second rates derived from counter deltas
- SRT send/receive buffer occupancy and flight size
- for bonded sockets, member counts and connected/active/broken state from
  `srt_group_data()`

RTMP publisher quality is read from the accepted Linux TCP socket with
`getsockopt(TCP_INFO)` and `getsockopt(SO_MEMINFO)`. It includes:

- RTT and receive RTT
- bytes received and receive-rate delta
- time since last receive
- receive window/space
- out-of-order packet count
- kernel receive-buffer allocation and limit

On non-Linux hosts the RTMP socket-specific fields report an explicit
unavailable reason.

## SRT Listener Pressure

All SRT ingests share the listener's kernel UDP receive buffer. A monitor reads
`/proc/net/udp` once per second and exports:

```json
{
  "srtListener": {
    "bondingAvailable": false,
    "udpRxQueueBytes": 0,
    "udpRxQueuePeakBytes": 0,
    "udpDrops": 0
  }
}
```

The diagnostic runner warns above 50% queue occupancy, alerts above 75%, and
reports any kernel drop count.

## Diagnostics Checks

The current SSE run emits:

1. Engine Status
2. Stream Info
3. GOP Analysis
4. Publisher Transport
5. Ring Buffer Health
6. Active Outputs
7. System Resources
8. Network Bandwidth
9. SRT Listener Socket

Some checks are contextual rather than packet-accurate. In particular, network
bandwidth is host-wide and current ring-buffer fill does not yet represent
per-reader lag.

Engine Status and Active Outputs associate egresses through the explicit
`ActiveEgress.pipeline_id` field.

See [Diagnostics](diagnostics.md) for the instrumentation plan and its explicit
measurement boundaries.

## Prometheus and Grafana Status

The old MediaMTX Prometheus/Grafana setup belongs to the archived implementation
under `old/`. The current Rust binary has no `/metrics` text endpoint and no
`/grafana` reverse proxy.

The frontend still contains Grafana links from the previous UI. They should be
treated as dormant compatibility UI until a Rust-native metrics exporter or an
external dashboard contract is implemented.

Recommended next step: export bounded process, pipeline, transport, ring-reader,
and egress counters in Prometheus format without putting labels or allocation
work on the packet hot path.
