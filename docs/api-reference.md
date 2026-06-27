# Restream API Reference

Base URL: `http://localhost:3030`

JSON uses camelCase. Unless noted otherwise, routes require the `session` cookie
returned by login.

## Request Limits

| Limit | Value |
|---|---|
| Maximum request body | 4 MiB |
| `name` / `serverName` / `label` fields | 256 bytes |
| `url` / output URL fields | 2048 bytes |
| `encoding` string | 512 bytes |
| `streamKey` | 256 bytes |
| `ffmpegArgs` (custom encoding) | 4096 bytes |
| `password` | 1024 bytes |

Requests exceeding the body limit receive `413 Payload Too Large`. Fields
exceeding the per-field limits receive `400 Bad Request` with a descriptive
message.

## Authentication

| Method | Route | Purpose |
|---|---|---|
| `POST` | `/api/auth/login` | Create a persisted session from `{ "password": "..." }` |
| `POST` | `/api/auth/logout` | Delete the current session |
| `POST` | `/api/auth/change-password` | Change the password; existing sessions remain valid |

Static pages/assets are served without an auth gate; protected API handlers
enforce the cookie themselves. `/health`, `/healthz`, and `/audio-caps` are
public. HLS pull routes require the dashboard session cookie.

All responses include `X-Content-Type-Options: nosniff` and
`X-Frame-Options: SAMEORIGIN` security headers. These are applied globally by
a `SetResponseHeaderLayer` on the main router.

## Configuration and Discovery

### `GET /config`

Returns SQLite-backed settings plus configured pipelines, outputs, and jobs.

```json
{
  "serverName": "Name",
  "ingestHost": "stream.example.com",
  "ingestSecurity": {
    "failureLimit": 10,
    "failureWindowMs": 60000,
    "banMs": 600000,
    "trackedIpLimit": 10000
  },
  "pipelines": [],
  "outputs": [],
  "jobs": []
}
```

Each pipeline in this response also includes generated RTMP and SRT ingest URLs.

### `PATCH /config`

Updates any supplied setting:

```json
{
  "serverName": "India Restream",
  "ingestHost": "stream.example.com",
  "ingestSecurity": {
    "failureLimit": 10,
    "failureWindowMs": 60000,
    "banMs": 600000,
    "trackedIpLimit": 10000
  }
}
```

An empty `serverName` returns `400`.

When `transcodeProfiles` are included in `PATCH /config`, each profile is
validated before saving:

- `preset` must be one of: `ultrafast`, `superfast`, `veryfast`, `faster`, `fast`, `medium`, `slow`, `slower`, `veryslow`, `placebo`
- `tune` must be empty or one of: `film`, `animation`, `grain`, `stillimage`, `psnr`, `ssim`, `fastdecode`, `zerolatency`
- `crf` must be in `0..=51`

Invalid values return `400 Bad Request` with a descriptive error.

### `GET /stream-keys`

Returns 20 built-in keys and native ingest URLs:

```json
[
  {
    "key": "stream-key",
    "label": "Stream 1",
    "ingestUrls": {
      "rtmp": "rtmp://stream.example.com:1935/live/stream-key",
      "srt": "srt://stream.example.com:10080?streamid=publish:live/stream-key"
    }
  }
]
```

There are no create/update/delete stream-key routes in the Rust router.

### `GET /audio-caps`

Returns the frontend's platform/protocol audio capability matrix.

## Pipelines

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/pipelines` | List pipelines |
| `POST` | `/pipelines` | Create a pipeline |
| `POST` | `/pipelines/:id` | Replace editable pipeline fields |
| `DELETE` | `/pipelines/:id` | Delete a pipeline |

Create/update body:

```json
{
  "name": "Main Feed",
  "streamKey": "stream-key",
  "inputSource": null,
  "encoding": null
}
```

`name` is required for both create and update because the current update handler
uses the same payload type. If `streamKey` is omitted on create, the first unused
built-in key is selected.

`inputSource` and pipeline-level `encoding` are persisted but are not used to
pull remote media or transform the active native ingest path.

Deletion cancels configured output tasks, the active ingest, and any
file-ingest FFmpeg subprocesses whose `streamKey` matches the pipeline's
stream key before removing the pipeline row. Shared transcoder, HLS, and
recording cleanup still follows their existing task lifecycle.

## Outputs

| Method | Route | Purpose |
|---|---|---|
| `POST` | `/pipelines/:pipelineId/outputs` | Create an output |
| `POST` | `/pipelines/:pipelineId/outputs/:outputId` | Update an output |
| `DELETE` | `/pipelines/:pipelineId/outputs/:outputId` | Delete an output |
| `POST` | `/pipelines/:pipelineId/outputs/:outputId/start` | Set `desiredState=running` |
| `POST` | `/pipelines/:pipelineId/outputs/:outputId/stop` | Set `desiredState=stopped` |

Create/update body:

```json
{
  "name": "Primary CDN",
  "url": "rtmp://destination.example/live/key",
  "encoding": "1080p+atrack:0"
}
```

The one-second reconciler starts and stops native egress tasks from
`desiredState`.

Output encoding accepts `source`, built-in video presets, and audio-routing
suffixes. `custom` output encoding is rejected with `400 Bad Request` because
custom FFmpeg arguments are persisted for future use but are not applied by the
runtime yet.

Deleting a running output cancels and unregisters its active egress before the
database row is removed.

URL behavior:

| URL prefix | Egress |
|---|---|
| `rtmp://` | RTMP |
| `rtmps://` | RTMPS with TLS before the RTMP handshake |
| `srt://` | SRT/MPEG-TS |
| `hls://` | Local in-memory HLS segmenter |
| `http://` | HLS HTTP PUT upload |
| `https://` | HLS HTTP PUT upload |

Any other prefix is rejected during validation with a `400 Bad Request`.
HTTP/HTTPS HLS upload uses one shared local segmenter per pipeline, PUTs each
new `seg<N>.ts`, then PUTs the playlist URL.

## History

| Method | Route | Response |
|---|---|---|
| `GET` | `/pipelines/:pipelineId/history` | `{ pipelineId, logs }` |
| `GET` | `/pipelines/:pipelineId/outputs/:outputId/history` | `{ pipelineId, outputId, logs }` |

The current handlers do not expose query filtering even though the DB layer has
filter support.

## Output Status

### `GET /pipelines/:pipelineId/outputs/:outputId/status`

Returns live egress telemetry for a single active output: `phase`, `bytesOut`,
`lastProgressAt`, `lastError`, `failurePhase`, `uptimeSecs`, `protocol`,
`targetAddr`, `quality`, and `metrics`. Returns `404` when the output is not
actively running.

## Probe, Graph, and Diagnostics

### `GET /pipelines/:pipelineId/probe`

Returns active native ingest metadata, audio tracks, bitrate, GOP observations,
and ingest identity. Returns `404` without an active ingest.

### `GET /pipelines/:pipelineId/graph`

Returns the current processing DAG: ingest, source ring, transcoder stages,
egresses, HLS, and recording nodes where present.

### `GET /pipelines/:pipelineId/diagnostics`

Streams Server-Sent Events. An optional `probe=rtmp|srt` query must match the
active ingest protocol. Returns `404` without an active ingest and `400` for a
protocol mismatch.

## Optional Agent Plane

The phase-4 agent read/planning plane is behind the `agent-plane` Cargo feature.
Normal core builds compile it out and return `404` from `/api/v1/agent/*`
routes with `compiledIn: false`.

When compiled with `--features agent-plane`, the routes are authenticated,
read-only, and do not mutate pipeline, output, or runtime state. Execution is
reserved for the separate `agent-execution` phase-6 feature.

The agent capability route catalog intentionally lists only agent-plane routes.
Core operator APIs may expose raw operator data such as target URLs; agents
should use `/api/v1/agent/context` and investigation responses for redacted
state.

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/v1/agent/capabilities` | Discover compiled-in read and planning tools |
| `GET` | `/api/v1/agent/context` | Return one redacted read-only state bundle for agent reasoning |
| `POST` | `/api/v1/agent/investigations` | Bundle health, graph, telemetry, alerts, and events for investigation workflows |
| `POST` | `/api/v1/agent/plans` | Convert intent plus structured proposed changes into a draft plan |
| `POST` | `/api/v1/agent/plans/validate` | Return only validation results for a draft plan |
| `POST` | `/api/v1/agent/graph-diff-preview` | Return graph/impact preview for a draft plan |

When compiled with `--features agent-execution`, the API also exposes
approval-gated operation routes. These routes are still authenticated, and
operation responses are redacted before they are returned.

| Method | Route | Purpose |
|---|---|---|
| `POST` | `/api/v1/agent/operations` | Create an operation object from an intent, structured changes, and optional idempotency key |
| `GET` | `/api/v1/agent/operations/:operation_id` | Read operation status, audit log, progress, execution result, and verification result |
| `POST` | `/api/v1/agent/operations/:operation_id/approve` | Record explicit approval before mutation is allowed |
| `POST` | `/api/v1/agent/operations/:operation_id/apply` | Apply approved output add/update/remove/start/stop changes through the core DB/runtime primitives |
| `POST` | `/api/v1/agent/operations/:operation_id/verify` | Verify post-change health, graph convergence, and alert delta |
| `POST` | `/api/v1/agent/verify` | Verify by body: `{ "operationId": "op_..." }` |

Without `agent-execution`, these operation routes return an authenticated `404`
with `feature: "agent-execution"` and `compiledIn: false`.

Context responses include:

- route and lightweight schema metadata for agent clients
- build/runtime status, OS basics, native-library versions, and feature flags
- redacted pipelines, outputs, ingests, jobs, transcode profiles, and settings
- current desired-vs-actual summaries for inputs, outputs, recording, and HLS
- health, engine telemetry, per-pipeline telemetry, processing graphs, alerts,
  and recent lifecycle events
- media inventory, storage summary, dependency summaries, and passive
  diagnostics findings plus active diagnostics route metadata
- redaction metadata describing which fields were removed

Raw stream keys and output URLs are never returned by this endpoint. They are
replaced with stable SHA-256 fingerprints plus URL scheme/host summaries.
The context endpoint does not open active diagnostics probes; agents can use the
advertised diagnostics SSE route when an explicit live probe is needed.

Plan request:

```json
{
  "intent": "Attach a 720p RTMP output",
  "pipelineId": "pipeline_abc",
  "proposedChanges": [
    {
      "kind": "addOutput",
      "name": "Primary CDN",
      "url": "rtmp://destination/live/key",
      "encoding": "720p+atrack:0"
    }
  ]
}
```

Operation create request:

```json
{
  "intent": "Attach a stopped RTMP output",
  "pipelineId": "pipeline_abc",
  "idempotencyKey": "change-ticket-123",
  "actor": "agent",
  "agentId": "ops-agent",
  "toolIdentity": "agent-execution-api",
  "incidentId": "incident_123",
  "incidentLinks": ["alert:egress-stale"],
  "proposedChanges": [
    {
      "kind": "addOutput",
      "name": "Primary CDN",
      "url": "rtmp://cdn.example/live/key",
      "encoding": "source",
      "desiredState": "stopped"
    }
  ]
}
```

Operation records include `operationId`, `status`, `approval`, `request`,
`plan`, `proposedPlanHash`, `incidentId`, `incidentLinks`, `affectedObjects`,
`stateTransitions`,
`progressSnapshots`, `auditLog`, `executionResult`, and `verificationResult`.
`apply` is rejected until approval is recorded.

Plan responses include `planId`, validation errors/warnings, static graph
preview, and impact notes. `executionEnabled` is `true` only when
`agent-execution` is compiled in.

The current run contains nine checks; see [Observability](observability.md).

## Recording

| Method | Route | Purpose |
|---|---|---|
| `POST` | `/pipelines/:pipelineId/recording/start` | Persist enabled state and start immediately if ingest is active |
| `POST` | `/pipelines/:pipelineId/recording/stop` | Disable and cancel recording |

Response:

```json
{ "enabled": true, "active": true }
```

The recording path writes raw MPEG-TS files in `media/`, and files whose task
lifetime is shorter than five seconds are deleted as transient artifacts. The
recording feeder uses the shared TS packet feeder before writing to the
MemoryQueue-backed file writer.

## File Ingest

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/ingests` | List configured file ingests |
| `POST` | `/api/ingests` | Create |
| `PUT` | `/api/ingests/:id` | Update |
| `DELETE` | `/api/ingests/:id` | Delete |
| `POST` | `/api/ingests/:id/start` | Start file ingest via the configured backend |
| `POST` | `/api/ingests/:id/stop` | Stop the active ingest task/process |

Create/update body:

```json
{
  "filename": "example.mp4",
  "streamKey": "stream-key",
  "loop": true,
  "startTime": "00:00:05"
}
```

Start returns `400` if `media/<filename>` does not exist, `400` if no pipeline
matches the configured stream key, and `409` if that ingest ID already has a
running file ingest or the target pipeline already has another active
publisher.

By default the backend is the embedded `public/bin/ffmpeg` subprocess:

```text
ffmpeg -re [-stream_loop -1] [-ss <start>] -i media/<file> -map 0 -c copy -f mpegts pipe:1
```

Set `RESTREAM_USE_INTERNAL_FILE_INGEST=1` to switch start/stop to the
in-process remux path instead. Deleting an ingest definition terminates the
running ingest regardless of backend.
Both stop and delete kill the child and call `wait()` to reap it immediately so
no zombie processes remain.

## Media Files

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/media` | List `.mkv`, `.mp4`, and `.mov` files in `media/` |
| `DELETE` | `/api/media/:filename` | Delete an unreferenced file under `media/` |

Deletion returns `409` when a configured file ingest references the filename.
Deletion canonicalizes both the `media/` root and the requested target path.
Requests that resolve outside `media/` (path traversal) return `400`.
Missing files return `404`.

## Custom Encoding

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/encodings/custom` | Return `{ "ffmpegArgs": "..." }` |
| `PUT` | `/encodings/custom` | Persist `{ "ffmpegArgs": "..." }` |

The value is configuration-only today. The native transcoder does not interpret
the stored FFmpeg argument string.

## Health and Status

### `GET /health`

Public native state snapshot:

```json
{
  "generatedAt": "2026-06-20T12:00:00Z",
  "status": "ready",
  "pipelines": {
    "pipeline_id": {
      "input": {
        "status": "on",
        "publishStartedAt": "2026-06-20T11:59:00Z",
        "bytesReceived": 12000000,
        "bitrateKbps": 1600,
        "video": {},
        "audio": {},
        "publisher": {
          "protocol": "srt",
          "remoteAddr": "203.0.113.10:50000",
          "quality": {
            "srtBonded": true,
            "srtGroupMemberCount": 2,
            "srtGroupConnectedMembers": 2,
            "srtGroupActiveMembers": 1,
            "srtGroupBrokenMembers": 0
          }
        }
      },
      "outputs": {},
      "recording": { "enabled": false, "active": false }
    }
  },
  "srtListener": {
    "bondingAvailable": false,
    "udpRxQueueBytes": 0,
    "udpRxQueuePeakBytes": 0,
    "udpDrops": 0
  }
}
```

See [Observability](observability.md) for field derivation, publisher quality,
and diagnostic check details.

### `GET /healthz`

Public liveness response:

```json
{ "status": "ok" }
```

### `GET /metrics/system`

Authenticated JSON containing CPU, memory, disk, and host-wide network rates.
This is not Prometheus text format.

### `GET /api/status`

Authenticated build/runtime information:

```json
{
  "restream": {
    "version": "0.1.0",
    "commit": "abc558b",
    "nativeBuildId": "..."
  },
  "toolchain": {
    "rustc": "1.96.0",
    "target": "x86_64-unknown-linux-gnu",
    "llvm": "22.1.2",
    "gccRuntime": "13.3.0"
  },
  "nativeLibraries": {
    "ffmpeg": {
      "version": "8.1.2",
      "configuration": "... --enable-x86asm ...",
      "license": "GPL version 2 or later",
      "x86Assembly": true
    },
    "srt": {
      "version": "1.5.5",
      "buildVersion": "1.5.5",
      "bondingAvailable": true
    },
    "openssl": {
      "version": "OpenSSL 3.0.x ...",
      "buildVersion": "3.0.x"
    },
    "sqlite": { "version": "3.x", "sourceId": "..." },
    "x264": {
      "version": "0.164.x",
      "versionSource": "linked pkg-config metadata at build time"
    },
    "x265": {
      "version": "3.x",
      "versionSource": "linked pkg-config metadata at build time"
    }
  },
  "sbom": {
    "format": "CycloneDX",
    "specVersion": "1.5",
    "endpoint": "/api/status/sbom",
    "componentCount": 100,
    "rustComponentCount": 85,
    "nativeComponentCount": 16,
    "licensesIncluded": true
  },
  "os": {
    "platform": "linux",
    "arch": "x86_64",
    "hostname": "host",
    "kernelVersion": "6.x",
    "uptime": 12345,
    "totalMem": 17179869184
  }
}
```

Native versions are obtained from the running libraries where they expose a
runtime API. x264 and x265 have no public runtime version call, so their exact
linked pkg-config versions are embedded at build time and labeled accordingly.

### `GET /api/status/sbom`

Authenticated CycloneDX 1.5 JSON software bill of materials. The response uses
content type `application/vnd.cyclonedx+json; version=1.5` and contains:

- the Restream application component and build identity;
- every resolved normal/runtime Rust crate from Cargo's locked dependency
  graph, including version, Cargo package URL, source, and declared license;
- FFmpeg component libraries, SRT, libssl, libcrypto, SQLite, x264, x265, glibc
  when applicable, Rust's standard library, libstdc++, and libgcc;
- runtime-reported versions where an API exists, with explicit provenance for
  build-resolved versions;
- SPDX license expressions or `NOASSERTION` when upstream metadata does not
  declare a license.

The SBOM describes software present in the running artifact. It intentionally
does not include development-only or benchmark dependencies.

## HLS Pull

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/hls/:pipelineId` | Playlist alias |
| `GET` | `/hls/:pipelineId/index.m3u8` | Playlist |
| `GET` | `/hls/:pipelineId/seg<N>.ts` | MPEG-TS segment |

`/preview/hls/...` remains as a deprecated compatibility alias.

Responses:

- playlist: `application/vnd.apple.mpegurl`
- segment: `video/mp2t`
- `404`: no active store, no completed segments, or evicted segment
- `400`: invalid segment filename

These routes require the dashboard session cookie.

All HLS routes respond with `Access-Control-Allow-Origin: *` and allow `GET`,
`OPTIONS`, `Content-Type`, and `Range` so browser-based players on other
origins can pull segments and playlists without CORS preflight errors.

## Operator v1 Endpoints

All `/api/v1` routes require the session cookie.

### `GET /api/v1/overview`

Engine-wide operator summary: pipeline counts, alert rollup, SRT listener state.

```json
{
  "generatedAt": "...",
  "totalPipelines": 3,
  "activePipelines": 2,
  "degradedPipelines": 0,
  "failedOutputs": 0,
  "alertCount": { "critical": 0, "warning": 1 },
  "srtListener": { ... }
}
```

### `GET /api/v1/alerts`

Aggregate alerts across all pipelines. Each alert carries `id`, `severity`,
`scope`, `evidence`, `recommendedAction`, `firstSeen`, and `lastSeen` fields.
Sorted Critical-first. `firstSeen` is stamped on first observation;
`lastSeen` updates on every subsequent observation. Resolved alerts are
pruned automatically.

```json
{
  "generatedAt": "...",
  "alerts": [ { "id": "...", "severity": "Warning", ... } ]
}
```

### `GET /api/v1/events`

Lifecycle event log. Query params: `pipeline_id` (optional filter),
`limit` (default 100, max 1000).

```json
{
  "generatedAt": "...",
  "events": [ { "seq": 1, "kind": "IngestConnected", "pipelineId": "...", "timestamp": "..." } ]
}
```

### `GET /api/v1/pipelines/:pipelineId/summary`

Operator-focused pipeline view: source state, output rollup, recording,
HLS preview, alerts. Returns 404 for unknown pipeline IDs.

### `GET /api/v1/pipelines/:pipelineId/alerts`

Alerts for a single pipeline. Same alert shape as the aggregate endpoint.

## Engineer v1 Endpoints

All engineer endpoints require the session cookie.

### `GET /api/v1/engine/telemetry`

Engine-wide telemetry: all active ingests, processing stages with throughput
counters, egresses, and transcoder buffer count.

```json
{
  "generatedAt": "...",
  "ingests": [
    { "pipelineId": "...", "protocol": "rtmp", "uptimeSecs": 42.5, "bytesReceived": 12345678, "metrics": { ... } }
  ],
  "stages": [
    { "stageKey": "pipe1:video:720p", "pipelineId": "pipe1", "kind": "video:720p", "metrics": { "packetsIn": 100, "packetsOut": 100, "bytesIn": 50000, "bytesOut": 30000, "processingUs": 1200 }, "pipeMetrics": { ... } }
  ],
  "egresses": [
    {
      "outputId": "...",
      "pipelineId": "...",
      "protocol": "rtmp",
      "targetUrl": "rtmp://...",
      "targetAddr": "203.0.113.10:1935",
      "status": "running",
      "phase": "sending",
      "uptimeSecs": 42.0,
      "bytesOut": 9876543,
      "lastProgressAt": "...",
      "lastProgressAgeMs": 250,
      "lastError": null,
      "lastErrorAt": null,
      "failurePhase": null,
      "quality": {
        "tcpCongestionAlgorithm": "cubic",
        "tcpRttMs": 12.4,
        "tcpSendRateMbps": 4.8,
        "tcpNotsentBytes": 0,
        "tcpSndCwnd": 10,
        "tcpTotalRetrans": 0,
        "mbpsSendRate": null,
        "srtBonded": null
      },
      "metrics": { ... }
    }
  ],
  "activeTranscoderBuffers": 2
}
```

### `GET /api/v1/pipelines/:pipelineId/telemetry`

Pipeline-scoped telemetry: ingest, source ring buffer, processing stages,
and egresses for a single pipeline.

```json
{
  "generatedAt": "...",
  "pipelineId": "...",
  "ingest": { "protocol": "srt", "uptimeSecs": 10.0, "bytesReceived": 500000, "metrics": { ... } },
  "sourceRing": { "fill": 42, "capacity": 8192, "readers": [ { "name": "...", "lagSlots": 5, "overflowCount": 0, "packetAgeMs": 120 } ] },
  "stages": [ { "kind": "video:720p", "metrics": { ... } } ],
  "egresses": [ { "outputId": "...", "uptimeSecs": 10.0, "bytesOut": 400000 } ]
}
```

### `GET /api/v1/stages/:stageKey/telemetry`

Single-stage telemetry by stage key (e.g. `pipe1:video:720p`). Returns raw
throughput counters and subprocess pipe metrics (if present). Returns 404
if the stage is not currently active.

```json
{
  "generatedAt": "...",
  "stageKey": "pipe1:video:720p",
  "pipelineId": "pipe1",
  "kind": "video:720p",
  "metrics": { "packetsIn": 100, "packetsOut": 100, "bytesIn": 50000, "bytesOut": 30000, "processingUs": 1200 },
  "pipeMetrics": null
}
```
