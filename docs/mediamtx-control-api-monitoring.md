# MediaMTX Monitoring Notes

This document is a short guide to how Restream uses MediaMTX runtime information and where future monitoring work should look. It intentionally does not duplicate the full MediaMTX API reference.

## Useful MediaMTX References

- [MediaMTX Control API reference](https://mediamtx.org/docs/references/control-api)
- [MediaMTX configuration reference](https://mediamtx.org/docs/references/configuration-file)
- [MediaMTX metrics](https://mediamtx.org/docs/usage/metrics)
- [MediaMTX pprof/performance monitoring](https://mediamtx.org/docs/usage/pprof)
- [MediaMTX architecture](https://mediamtx.org/docs/features/architecture)

Use those pages for current endpoint shapes, available fields, and version-specific behavior.

## What Restream Uses Today

Restream currently uses MediaMTX for three control-plane jobs:

| Area | MediaMTX surface | Why Restream uses it |
|---|---|---|
| Readiness and ingest URL discovery | `GET /v3/config/global/get` | Confirms the API is reachable and reads active protocol ports |
| Stream key provisioning | `POST /v3/config/paths/add/{name}`, `DELETE /v3/config/paths/delete/{name}` | Creates and removes `live/<streamKey>` path config |
| Health snapshot | `GET /v3/paths/list`, `GET /v3/rtmpconns/list`, `GET /v3/srtconns/list` | Builds input status, publisher identity, and protocol quality signals |

The health model is intentionally ingest-centric today. Details are in [health-mapping.md](./health-mapping.md).

## Monitoring Model

Restream should keep MediaMTX monitoring split into three layers:

| Layer | Purpose | Shape |
|---|---|---|
| Health summary | Fast dashboard polling | Compact `/health` payload with pipeline status, output status, publisher protocol, quality flags, and aggregate counts |
| Drilldown | Operator investigation | Focused endpoints for one path/session/connection using MediaMTX `get/{id}` or `get/{name}` APIs |
| Admin actions | Explicit remediation | Optional guarded actions such as kicking a stuck session |

The key design point: do not put every MediaMTX field into `/health`. Keep `/health` small and add drilldown views when the UI needs detail.

## High-Value Future Additions

### MediaMTX Uptime And Version

Add `GET /v3/info` to detect MediaMTX restarts, expose version, and help correlate stream issues with server restarts.

### Runtime Drift Checks

Use config read endpoints for operator warnings, not automatic mutation:

- `GET /v3/config/global/get`
- `GET /v3/config/pathdefaults/get`
- `GET /v3/config/paths/list`

Useful checks include API disabled, unexpected port changes, missing HLS settings, unexpected path overrides, recording enabled on only some paths, or auth settings that differ from the expected deployment shape.

### Protocol Drilldown

Add backend drilldown endpoints instead of expanding `/health` indefinitely:

- one MediaMTX path
- one RTMP connection
- one SRT connection
- one RTMP connection
- one SRT connection
- one HLS muxer, if HLS playback monitoring matters

These should normalize MediaMTX details into Restream terms such as pipeline, stream key, publisher, reader, output, and warning.

### SRT Quality

SRT is one of the richest MediaMTX telemetry surfaces. If SRT ingest becomes important, expose more of its network-health counters in drilldown views: RTT, receive/send rate, loss, retransmits, drops, undecrypt failures, and buffer pressure.

### HLS Playback Visibility

If dashboard preview or external HLS playback becomes operationally important, use HLS muxer runtime APIs to show whether a path has active playback, when it was last requested, and whether frames are being discarded.

### Remediation Actions

MediaMTX exposes kick endpoints for protocol sessions and connections. These are useful, but should stay behind explicit admin action and should not be part of background monitoring.

## Adjacent Observability

The Control API is good for object state: paths, sessions, connections, muxers, config, and recordings.

For time-series observability, use MediaMTX metrics instead. MediaMTX exposes Prometheus-compatible metrics when enabled in config; those are a better fit for dashboards and alerts over time.

For CPU, memory, and goroutine investigation, use MediaMTX pprof when enabled. That belongs in an operator/debug workflow, not in Restream's normal polling loop.

## Current Priority

1. Add `GET /v3/info` to the health service for restart/version visibility.
2. Add compact aggregate protocol counters to `/health`.
3. Add targeted drilldown endpoints before adding more fields to `/health`.
4. Add HLS/SRT detail only when those workflows become operationally important.
5. Keep write-side MediaMTX actions explicit and admin-only.
