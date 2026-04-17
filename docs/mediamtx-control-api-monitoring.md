# MediaMTX Control API Monitoring Opportunities

This document reviews the MediaMTX control API in `api/openapi.yaml` and maps it to Restream's current implementation and monitoring gaps.

## Current State In This Repo

Restream currently uses a focused but growing subset of the MediaMTX control API:

- Readiness: `GET /v3/config/global/get`
- Stream key path provisioning: `POST /v3/config/paths/add/{name}` and `DELETE /v3/config/paths/delete/{name}`
- Runtime health snapshot: `GET /v3/paths/list`, `GET /v3/rtspconns/list`, `GET /v3/rtspsessions/list`, `GET /v3/rtmpconns/list`, `GET /v3/srtconns/list`, and `GET /v3/webrtcsessions/list`

That currently gives the project:

- input availability by path
- RTSP reader correlation for FFmpeg outputs
- publisher protocol and remote address visibility (RTSP/RTMP/SRT/WebRTC)
- ingest-quality counters for RTP and SRT publisher sessions
- basic per-path bytes, readers, and codec metadata

Gaps still remain around playback-focused telemetry (HLS muxers and WebRTC viewer-centric diagnostics), secure protocol variants (RTMPS/RTSPS), and dedicated drilldown APIs.

## Inventory Of MediaMTX Control API Endpoints

### General

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/info` | Returns instance information such as version and start time. | High. Use for MediaMTX uptime, version drift, and restart detection. |

### Authentication

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `POST /v3/auth/jwks/refresh` | Manually refreshes JWT JWKS. | Low for monitoring. Operational control only. |

### Global And Path Configuration

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/config/global/get` | Returns the active global runtime config. | High. Use for config drift checks and to confirm `api`, `metrics`, `pprof`, `webrtc`, `hls`, `playback`, and protocol settings. |
| `PATCH /v3/config/global/patch` | Patches global config. | Low for passive monitoring. Potentially useful for operator remediation, but risky to automate. |
| `GET /v3/config/pathdefaults/get` | Returns default path config. | Medium. Use to show inherited defaults that affect monitoring, recording, on-demand behavior, and reader limits. |
| `PATCH /v3/config/pathdefaults/patch` | Patches default path config. | Low for monitoring. Control plane action only. |
| `GET /v3/config/paths/list` | Lists all explicit path configs. | High. Use to detect orphaned paths, per-path overrides, recording policy, source mode, and auth differences. |
| `GET /v3/config/paths/get/{name}` | Returns one explicit path config. | High. Use for pipeline drilldown and config-versus-runtime diff views. |
| `POST /v3/config/paths/add/{name}` | Adds a path config. | Already used for provisioning. Not a telemetry endpoint. |
| `PATCH /v3/config/paths/patch/{name}` | Patches a path config. | Low for monitoring. Control plane action only. |
| `POST /v3/config/paths/replace/{name}` | Replaces a path config. | Low for monitoring. Control plane action only. |
| `DELETE /v3/config/paths/delete/{name}` | Deletes a path config. | Already used for provisioning. Not a telemetry endpoint. |

### Runtime Paths

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/paths/list` | Lists runtime path state. | Very high. This is the main path-level telemetry surface. It exposes source type, availability, online state, tracks, readers, and byte counters. |
| `GET /v3/paths/get/{name}` | Returns one runtime path. | Very high. Use for pipeline drilldown instead of scanning the full list for every detail request. |

Important runtime fields on `Path` objects:

- `source.type` and `source.id` identify whether the active publisher is RTMP, RTSP, RTSPS, SRT, WebRTC, HLS source, redirect, camera source, and so on.
- `available`, `availableTime`, `online`, and `onlineTime` let us distinguish publisher connected, stream ready, and stream offline states.
- `tracks2` exposes codec, dimensions, profile, level, channel count, and sample rate for most streams.
- `inboundBytes`, `outboundBytes`, and `inboundFramesInError` allow per-path throughput and ingest error tracking.
- `readers` gives a live list of reader types and IDs that can be correlated with protocol-specific endpoints.

### HLS Runtime

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/hlsmuxers/list` | Lists active HLS muxers. | High if HLS is enabled. Gives path, creation time, last request, bytes sent, and discarded frames. |
| `GET /v3/hlsmuxers/get/{name}` | Returns one HLS muxer. | High if HLS is enabled. Good for per-path playback drilldown. |

Important runtime fields on `HLSMuxer` objects:

- `created` indicates when HLS playback started.
- `lastRequest` indicates whether a viewer has requested the muxer recently.
- `outboundBytes` and `outboundFramesDiscarded` allow playback throughput and drop tracking.

### RTSP And RTSPS Runtime

The control API splits RTSP into connection and session resources. Sessions are more useful for monitoring because they expose publish or read state plus transport quality counters.

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/rtspconns/list` | Lists RTSP TCP control connections. | Medium to high. Useful for reader correlation and connection inventory. Already used. |
| `GET /v3/rtspconns/get/{id}` | Returns one RTSP connection. | Medium. Good for drilldown. |
| `GET /v3/rtspsessions/list` | Lists RTSP sessions. | Very high. Exposes session state, transport, path, user, jitter, loss, RTCP, and discard counters. Already used only as a fallback correlation source today. |
| `GET /v3/rtspsessions/get/{id}` | Returns one RTSP session. | Very high. Good for output and ingest drilldown. |
| `POST /v3/rtspsessions/kick/{id}` | Kicks an RTSP session. | Medium. Useful for operator remediation after bad sessions are detected. |
| `GET /v3/rtspsconns/list` | Lists RTSPS control connections. | Medium. Needed only if secure RTSP is enabled. |
| `GET /v3/rtspsconns/get/{id}` | Returns one RTSPS connection. | Medium. Drilldown only. |
| `GET /v3/rtspssessions/list` | Lists RTSPS sessions. | High when RTSPS is enabled. Same monitoring value as RTSP sessions. |
| `GET /v3/rtspssessions/get/{id}` | Returns one RTSPS session. | High when RTSPS is enabled. |
| `POST /v3/rtspssessions/kick/{id}` | Kicks an RTSPS session. | Medium. Operator remediation. |

Important runtime fields on `RTSPSession` objects:

- `state` tells whether the session is idle, reading, or publishing.
- `transport` distinguishes UDP, TCP, or multicast transport behavior.
- `path`, `query`, and `user` allow correlation to a pipeline, output, or external client.
- `inboundRTPPacketsLost`, `inboundRTPPacketsInError`, and `inboundRTPPacketsJitter` expose ingest quality issues.
- `outboundRTPPacketsReportedLost` and `outboundRTPPacketsDiscarded` expose downstream delivery problems.
- `conns` links a session back to one or more connection IDs.

### RTMP And RTMPS Runtime

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/rtmpconns/list` | Lists RTMP connections. | Very high. This is a key ingest telemetry surface and is now part of baseline health collection. |
| `GET /v3/rtmpconns/get/{id}` | Returns one RTMP connection. | High. Useful for drilldown and diagnostics. |
| `POST /v3/rtmpconns/kick/{id}` | Kicks an RTMP connection. | Medium. Useful for operator remediation. |
| `GET /v3/rtmpsconns/list` | Lists RTMPS connections. | High if secure RTMP is enabled. |
| `GET /v3/rtmpsconns/get/{id}` | Returns one RTMPS connection. | High if secure RTMP is enabled. |
| `POST /v3/rtmpsconns/kick/{id}` | Kicks an RTMPS connection. | Medium. Operator remediation. |

Important runtime fields on `RTMPConn` objects:

- `state` distinguishes idle, read, and publish.
- `path` shows which stream key is active.
- `query` and `user` can carry client correlation or auth context.
- `remoteAddr` allows source IP visibility and potential allowlist validation.
- `inboundBytes`, `outboundBytes`, and `outboundFramesDiscarded` provide coarse throughput and delivery health.

### SRT Runtime

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/srtconns/list` | Lists SRT connections. | Very high. This is the richest per-session telemetry surface in the whole control API. |
| `GET /v3/srtconns/get/{id}` | Returns one SRT connection. | Very high. Ideal for detailed diagnostics. |
| `POST /v3/srtconns/kick/{id}` | Kicks an SRT connection. | Medium. Useful for operator remediation. |

Important runtime fields on `SRTConn` objects:

- `state`, `path`, `query`, `user`, and `remoteAddr` identify the client and its role.
- `msRTT`, `mbpsSendRate`, `mbpsReceiveRate`, and `mbpsLinkCapacity` expose live network quality.
- `packetsReceivedLoss`, `packetsReceivedRetrans`, `packetsReceivedDrop`, and `packetsReceivedUndecrypt` expose loss, retransmit, drop, and decryption failure behavior.
- `bytesAvailSendBuf`, `bytesAvailReceiveBuf`, `packetsFlightSize`, `msSendBuf`, and `msReceiveBuf` expose congestion and buffer pressure.
- `outboundFramesDiscarded` exposes downstream frame drops.

For Restream specifically, SRT session telemetry is now collected in baseline health, but richer congestion and buffer analytics are still a gap.

### WebRTC Runtime

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/webrtcsessions/list` | Lists WebRTC sessions. | Very high. This is a key session telemetry surface; baseline collection exists, while viewer-centric playback breakdown remains a gap. |
| `GET /v3/webrtcsessions/get/{id}` | Returns one WebRTC session. | Very high. Good for drilldown and ICE diagnostics. |
| `POST /v3/webrtcsessions/kick/{id}` | Kicks a WebRTC session. | Medium. Useful for operator remediation. |

Important runtime fields on `WebRTCSession` objects:

- `peerConnectionEstablished` distinguishes failed negotiation from active media flow.
- `localCandidate` and `remoteCandidate` expose selected network paths and NAT behavior.
- `state`, `path`, `query`, and `user` identify the viewer or publisher and target stream.
- `inboundRTPPacketsLost`, `inboundRTPPacketsJitter`, `outboundFramesDiscarded`, and byte counters expose playback quality.

For Restream specifically, WebRTC monitoring is also a major gap because WebRTC playback is enabled in the current MediaMTX config and the dashboard does not surface active WebRTC viewers at all.

### WebRTC Ingest Restriction (Playback-Only Policy)

Based on the MediaMTX configuration reference, there is no `mediamtx.yml` setting that allows WebRTC playback while selectively denying WebRTC publish.

What is possible in config-only mode:

- `webrtc: true|false` enables or disables WebRTC entirely.
- `authInternalUsers` permissions can restrict by `action` and optional `path`, but not by `protocol`.
- `webrtcDisable` can disable WebRTC on a path, but this disables both WebRTC publish and WebRTC playback for that path.

Therefore, if we need to allow WebRTC playback but reject WebRTC ingest, `mediamtx.yml` alone is not sufficient.

Supported workaround at configuration level:

- Use `authMethod: http` and evaluate auth requests with a local callback endpoint.
- Reject requests where `action=publish` and `protocol=webrtc`.
- Allow `action=read` / `action=playback` for WebRTC viewers.

Operational notes for this repo:

- In container profile (shared network namespace), `authHTTPAddress: http://localhost:3030/...` works.
- In host profile (MediaMTX in its own container), callback routing to the app requires host-accessible addressing (for example `host.docker.internal`) instead of container-local `localhost`.
- This adds an auth round-trip for each connection attempt, so availability of the callback endpoint becomes part of ingest reliability.

### Recordings Runtime

| Endpoint | Purpose | Monitoring value |
| --- | --- | --- |
| `GET /v3/recordings/list` | Lists recordings grouped by path. | High if recording is enabled. Use for recording coverage, retention validation, and storage growth visibility. |
| `GET /v3/recordings/get/{name}` | Returns recordings for one path. | High if recording is enabled. Good for per-pipeline drilldown. |
| `DELETE /v3/recordings/deletesegment` | Deletes a recording segment. | Medium. Useful for retention tooling, not passive monitoring. |

Important runtime fields on `Recording` objects:

- `name` identifies the path.
- `segments` and each segment `start` time allow gap detection and retention visibility.

## What Restream Already Covers

Restream already has a good foundation around runtime path health and FFmpeg output correlation:

- `src/index.js` builds a cached health snapshot from `paths`, `rtspconns`, `rtspsessions`, `rtmpconns`, `srtconns`, and `webrtcsessions`.
- `docs/health-mapping.md` documents the current input and output status derivation model.
- `public/render.js` renders publisher protocol/remote badges plus ingest-side quality status and modal details.

The main limitation is scope. Current health is ingest-centric and still leaves some playback and secure-protocol runtime resources unused.

## Highest-Value Monitoring Additions

### 1. Expand protocol-aware ingest monitoring

These collectors are now in use for baseline ingest visibility:

- `GET /v3/rtmpconns/list`
- `GET /v3/srtconns/list`
- `GET /v3/webrtcsessions/list`

Next ingest-focused improvements are:

- `GET /v3/rtmpsconns/list` when secure RTMP is enabled
- `GET /v3/rtspssessions/list` when secure RTSP is enabled
- richer SRT congestion/buffer metrics (for example `bytesAvailReceiveBuf`, `msReceiveBuf`) in dashboard drilldowns

This would let the dashboard answer questions it cannot answer today:

- Is this input currently being published over RTMP, SRT, RTSP, or WebRTC?
- What is the publisher remote IP?
- Is the session publishing but unhealthy due to jitter, packet loss, or retransmit spikes?
- Are we seeing unexpected readers or publishers on a path?

### 2. WebRTC and HLS playback monitoring

Add or expand these collectors when the relevant MediaMTX features are enabled:

- `GET /v3/hlsmuxers/list`

This would add missing viewer telemetry:

- active WebRTC viewers per path (separate from ingest publishers)
- ICE establishment success and failure signals
- selected candidates for NAT troubleshooting
- HLS last-request timestamps for stale or abandoned viewers
- playback discard counters

### 3. Per-path and per-session drilldown endpoints in Restream

Instead of pushing every detail into the existing `/health` payload, expose focused backend endpoints that proxy and normalize MediaMTX data:

- `/runtime/paths/:streamKey`
- `/runtime/paths/:streamKey/sessions`
- `/runtime/connections/:protocol/:id`

These endpoints can use:

- `GET /v3/paths/get/{name}`
- protocol-specific `get/{id}` endpoints

This keeps `/health` compact while still enabling a detailed operator UI.

### 4. Config drift and feature-state monitoring

Use the config read endpoints to surface runtime drift:

- `GET /v3/info`
- `GET /v3/config/global/get`
- `GET /v3/config/pathdefaults/get`
- `GET /v3/config/paths/list`

Examples:

- MediaMTX restarted unexpectedly since the last poll.
- `metrics` is disabled even though external scraping is expected.
- `webrtcAdditionalHosts` does not match the host that browsers need.
- a path override enables recording or `maxReaders` on one stream but not others.

### 5. Controlled operator remediation

Kick endpoints are not passive monitoring, but they are operationally useful once session quality is visible:

- `POST /v3/rtspsessions/kick/{id}`
- `POST /v3/rtspssessions/kick/{id}`
- `POST /v3/rtmpconns/kick/{id}`
- `POST /v3/rtmpsconns/kick/{id}`
- `POST /v3/srtconns/kick/{id}`
- `POST /v3/webrtcsessions/kick/{id}`

These should be admin-only and never part of the normal polling loop.

## Recommended Priority For This Project

### P0

- Add `GET /v3/info` to detect MediaMTX restarts and expose version and uptime.
- Add aggregate protocol counters in `/health` (publishers/readers/viewers) for fast fleet-level visibility.
- Expand publisher drilldown fields (for example SRT buffer and congestion metrics) in the normalized health payload.

### P1

- Add protocol-specific drilldown views backed by `get/{id}` endpoints.
- Add HLS playback monitoring if HLS is enabled in this deployment.
- Add config drift checks from `config/global/get`, `config/pathdefaults/get`, and `config/paths/list`.

### P2

- Add operator remediation actions using kick endpoints.
- Add recordings monitoring if recording is enabled later.
- Use patch endpoints only for explicit admin workflows, not for background automation.

## A Practical Monitoring Shape For Restream

The cleanest model is to separate monitoring into three layers:

### Layer 1: Lightweight health snapshot

Keep `/health` as a compact summary:

- MediaMTX version, started time, and readiness
- per-pipeline source protocol, status, readers, bitrate, and key quality counters
- aggregate counts by protocol: RTMP publishers, SRT publishers, RTSP readers, WebRTC viewers, HLS viewers
- warning flags for loss, jitter, retransmits, decrypt failures, and discarded frames

### Layer 2: Drilldown API

Add normalized endpoints for operator views:

- one path
- one publisher session
- one viewer session
- one path config
- one recording inventory

### Layer 3: Admin control

Keep write-side MediaMTX actions behind explicit operator intent:

- kick a stuck session
- patch config
- delete a recording segment

## Outside The Control API But Worth Using

The OpenAPI document exposes global config fields for `metrics`, `metricsAddress`, `pprof`, `playback`, and related settings, but the metrics and profiling endpoints themselves are not part of this control API spec.

For full observability, Restream should treat these as adjacent surfaces:

- use `GET /v3/config/global/get` to verify whether metrics and pprof are enabled
- scrape MediaMTX metrics separately when enabled
- keep control API polling focused on object inventory and per-session state

## Bottom Line

The control API is already rich enough to take Restream from path-only health to protocol-aware session monitoring.

The biggest remaining wins are:

1. `GET /v3/info` and config read endpoints for restart and drift detection
2. protocol-specific drilldown backed by `get/{id}` endpoints
3. viewer-centric playback visibility (WebRTC/HLS)
4. secure protocol parity (RTMPS/RTSPS monitoring)
5. richer SRT congestion and buffer analytics in UI and alerts

If we implement only those first, Restream's monitoring model becomes substantially more accurate without needing to redesign the whole control plane.