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
enforce the cookie themselves. `/healthz` and `/audio-caps` are public.
HLS pull routes require the dashboard session cookie.

All responses include `X-Content-Type-Options: nosniff` and
`X-Frame-Options: SAMEORIGIN` security headers. These are applied globally by
a `SetResponseHeaderLayer` on the main router.

## Configuration and Discovery

The canonical authenticated settings surface is `/api/v1/settings`.

### `GET /api/v1/settings`

Returns SQLite-backed settings plus configured pipelines, outputs, and jobs.
Query params:

| Param | Default | Notes |
| --- | --- | --- |
| `jobs` | `all` | `latest` returns only the newest job per `(pipelineId, outputId)` pair for consumers that need a slimmed job list. |
| `view` | `full` | `dashboard` trims admin-only settings fields (`ingestSecurity`, `recordingSettings`, `srtIngest`) and omits job rows from the dashboard runtime fetch while keeping editor/runtime fields such as `ingestHost`, `transcodeProfiles`, pipelines, and outputs. Settings mode upgrades itself back to `full` on entry. |

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
  "recordingSettings": {
    "retainSourceTs": false
  },
  "pipelines": [],
  "outputs": [],
  "jobs": []
}
```

Each pipeline in this response also includes generated RTMP and SRT ingest URLs.

### `PATCH /api/v1/settings`

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
  },
  "recordingSettings": {
    "retainSourceTs": false
  }
}
```

An empty `serverName` returns `400`.

When `transcodeProfiles` are included in `PATCH /api/v1/settings`, each profile
is validated before saving:

- `preset` must be one of: `ultrafast`, `superfast`, `veryfast`, `faster`, `fast`, `medium`, `slow`, `slower`, `veryslow`, `placebo`
- `tune` must be empty or one of: `film`, `animation`, `grain`, `stillimage`, `psnr`, `ssim`, `fastdecode`, `zerolatency`
- `crf` must be in `0..=51`

Invalid values return `400 Bad Request` with a descriptive error.

### `GET /api/v1/stream-keys`

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
| `GET` | `/api/v1/pipelines` | List pipelines |
| `POST` | `/api/v1/pipelines` | Create a pipeline |
| `PATCH` | `/api/v1/pipelines/:id` | Replace editable pipeline fields |
| `DELETE` | `/api/v1/pipelines/:id` | Delete a pipeline |

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
| `POST` | `/api/v1/pipelines/:pipelineId/outputs` | Create an output |
| `PATCH` | `/api/v1/pipelines/:pipelineId/outputs/:outputId` | Update an output |
| `DELETE` | `/api/v1/pipelines/:pipelineId/outputs/:outputId` | Delete an output |
| `POST` | `/api/v1/pipelines/:pipelineId/outputs/:outputId/start` | Set `desiredState=running` |
| `POST` | `/api/v1/pipelines/:pipelineId/outputs/:outputId/stop` | Set `desiredState=stopped` |

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

## Process Logs

| Method | Route | Response |
|---|---|---|
| `GET` | `/api/logs` | `{ logs, total, hasMore }` |
| `GET` | `/api/logs/stream` | SSE stream (`event: log` frames) |

All process log entries are stored in the `app_logs` SQLite table and served
through these two endpoints. The frontend history UI calls `/api/logs` with
`pipeline_id`/`output_id` filters instead of relying on the pipeline-scoped
history endpoints.

### `GET /api/logs`

Query parameters:

| Parameter | Default | Description |
|---|---|---|
| `level` | `info` | Minimum level: `error`, `warn`, `info`, `debug` |
| `since` | — | RFC3339 lower bound (inclusive) |
| `until` | — | RFC3339 upper bound (exclusive) |
| `target` | — | Module prefix filter (`restream::media::srt`) |
| `pipeline_id` | — | Restrict to a single pipeline |
| `output_id` | — | Restrict to a single output (requires `pipeline_id`) |
| `event_class` | — | `lifecycle` to return only lifecycle transition events |
| `prefix` | — | Comma-separated message prefix filter (`stderr,exit`) |
| `limit` | `200` | 1–1000 |
| `order` | `desc` | `asc` or `desc` on `ts` |

Each log entry in the response includes `id`, `ts`, `level`, `target`,
`message`, `fields` (JSON), `pipelineId`, `outputId`, `eventType`.

### `GET /api/logs/stream`

SSE live tail. Accepts the same filter parameters as `GET /api/logs`.
On connect, the handler backfills entries newer than the `Last-Event-ID`
header (or `?last_event_id=`) from the database, then streams new entries
from the broadcast channel. A `": ping"` comment is sent every 20 seconds.
Lagging receivers are closed; the browser reconnects automatically using
`Last-Event-ID`.
The dashboard overview activity rail uses an initial `GET /api/logs` snapshot
plus this SSE endpoint filtered with `scope=restream` for live restream-wide
activity updates. Overview also reuses that same restream-scoped stream to
wake runtime summary refreshes on lifecycle events, avoiding a second
lifecycle-only SSE connection in that mode.
Pipeline, inspect, control-room, and publisher-health runtime surfaces
subscribe to this SSE endpoint with `event_class=lifecycle` so they refresh
immediately on process lifecycle transitions instead of waiting for the next
periodic poll.
Settings, media, and status also keep a narrower restream-scoped
`event_class=lifecycle` feed open so the global Rust-process indicator can
react to shutdown/fault/ready events without waking the heavier runtime health
polls in those modes.
The output-history and pipeline-history "Live" views use the same SSE endpoint
with `pipeline_id`, `output_id`, and `event_class` filters plus `Last-Event-ID`
resume cursors instead of periodic history re-polls.
Status mode also layers the same `scope=restream` stream over its initial
snapshot so restream process activity can update live without repeated log GETs.
Hidden dashboard tabs now close these SSE feeds and resume from the last seen
event id when visible again, falling back to slower snapshot polling only while
the tab is backgrounded.

## Output Status

Dashboard output start/stop controls update their button/card state
optimistically as soon as the API request is accepted, then let the next
runtime refresh confirm the actual engine state. That keeps control feedback
immediate without requiring a dedicated per-output poller.

### `GET /api/v1/pipelines/:pipelineId/outputs/:outputId/status`

Returns live egress telemetry for a single output. While active, this is the
current runtime state. After teardown/cleanup, the endpoint preserves the most
recent classified output snapshot, including `status`, `phase`, `lastError`,
`failurePhase`, `endedAt`, and active retry-backoff fields such as
`retrying`/`nextRetryAt`, so failure cleanup does not erase operator context.
Returns `404` only when the output has no active or recent runtime state.

Recovered outputs also expose short-lived downstream instability signals:
`recentFailureCount` tracks recent egress failures still inside the flap
window, and `flapping` becomes `true` after repeated sink failures even if the
output is currently back to `status=running`.

`GET /api/v1/engine/health` carries the complementary ingest-side instability
signal. In addition to `disconnectGraceActive` / `disconnectGraceRemainingMs`,
the input snapshot now includes `recentDisconnectCount` and `flapping` so
clients can distinguish a single recent drop from repeated reconnect churn.

## Probe, Graph, and Diagnostics

### `GET /api/v1/pipelines/:pipelineId/probe`

Returns active native ingest metadata, bitrate, GOP observations, and ingest
identity. Video and audio metadata include MPEG-TS `pid`, `language`, and
`title` when the source descriptors provide them. `audioTracks` lists every
active audio track; `audio` remains the primary/first track for older clients.
Returns `404` without an active ingest.

### `GET /api/v1/pipelines/:pipelineId/graph`

Returns the current processing DAG: ingest, source ring, transcoder stages,
egresses, HLS, and recording nodes where present.

### `GET /api/v1/pipelines/:pipelineId/diagnostics`

Streams Server-Sent Events. An optional `probe=rtmp|srt|file` query must match the
active ingest protocol. Returns `404` without an active ingest and `400` for a
protocol mismatch.

RTMP and SRT inputs run the transport-oriented checks documented in
[Observability](observability.md). File inputs switch to file-aware checks:
source-file presence and analysis, file-ingest runtime state, and preview /
recording readiness.

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
| `POST` | `/api/v1/pipelines/:pipelineId/recording/start` | Persist enabled state and start immediately if ingest is active |
| `POST` | `/api/v1/pipelines/:pipelineId/recording/stop` | Disable and cancel recording |

Response:

```json
{ "enabled": true, "active": true }
```

The recording path writes raw MPEG-TS files in `media/`, and files whose task
lifetime is shorter than five seconds are deleted as transient artifacts. The
recording feeder uses the shared TS packet feeder before writing to the
MemoryQueue-backed file writer.

When a recording stops successfully and is at least five seconds long, the
runtime starts a one-off FFmpeg remux from the source `.ts` into a sibling
`.mp4`. The media library prefers the `.mp4` for browser playback, keeps the
original `.ts` available for download while it exists, and surfaces
`conversionStatus` as `converting`, `ready`, or `failed`.

The deployment-wide setting `recordingSettings.retainSourceTs` controls whether
the original `.ts` is kept after a successful remux:

- `false` (default): delete the source `.ts` only after the `.mp4` is created successfully
- `true`: keep both files

Failed remuxes keep the source `.ts` regardless of this setting.

## File Ingest

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/v1/ingests` | List configured file ingests |
| `POST` | `/api/v1/ingests` | Create |
| `PUT` | `/api/v1/ingests/:id` | Update |
| `DELETE` | `/api/v1/ingests/:id` | Delete |
| `POST` | `/api/v1/ingests/:id/start` | Start file ingest via the configured backend |
| `POST` | `/api/v1/ingests/:id/stop` | Stop the active ingest task/process |

Create/update body:

```json
{
  "filename": "example.mp4",
  "streamKey": "stream-key",
  "loop": true,
  "startTime": "00:00:05",
  "liveOptimized": true,
  "targetGopSeconds": 2
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

Set `RESTREAM_USE_INTERNAL_FILE_INGEST=1` to switch passthrough
`liveOptimized=false` starts to the in-process remux path instead.

When `liveOptimized=true`, start always uses the embedded FFmpeg subprocess and
re-encodes toward a live-friendly GOP cadence:

- video: H.264 (`libx264`)
- audio: AAC
- forced keyframes every `targetGopSeconds`
- scene-cut GOP drift disabled

Deleting an ingest definition terminates the running ingest regardless of backend.
Both stop and delete kill the child and call `wait()` to reap it immediately so
no zombie processes remain.

## Media Files

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/v1/media` | List supported media files in `media/` |
| `GET` | `/api/v1/media/:filename/analysis` | Return source-file codec / duration / GOP analysis |
| `PATCH` | `/api/v1/media/:filename` | Rename a media file without changing its extension |
| `DELETE` | `/api/v1/media/:filename` | Delete an unreferenced file under `media/` |

Deletion returns `409` when a configured file ingest references the filename.
Deletion canonicalizes both the `media/` root and the requested target path.
Requests that resolve outside `media/` (path traversal) return `400`.
Missing files return `404`.

`GET /api/v1/media` returns entries for `.ts`, `.mkv`, `.mp4`, and `.mov`
files. Recording-backed entries may include:

- `sourceName` / `sourceSize`
- `convertedName` / `convertedSize`
- `playName`
- `conversionStatus`
- `conversionError`
- `conversionUpdatedAt`

For recordings with a successful `.mp4` remux, `playName` points at the `.mp4`
while `sourceName` still refers to the original recording `.ts`.

Renaming keeps the file extension fixed. For recording source `.ts` files, the
server also renames any sibling converted `.mp4` and conversion-state JSON, and
updates configured file-ingest rows that referenced the old filename.

## Custom Encoding

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/v1/encodings/custom` | Return `{ "ffmpegArgs": "..." }` |
| `PUT` | `/api/v1/encodings/custom` | Persist `{ "ffmpegArgs": "..." }` |

The value is configuration-only today. The native transcoder does not interpret
the stored FFmpeg argument string.

## Health and Status

### `GET /api/v1/engine/health`

Authenticated native state snapshot:

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
      "outputs": {
        "output_id": {
          "status": "failed",
          "phase": "failed",
          "lastError": "connection reset by peer",
          "failurePhase": "send",
          "endedAt": "2026-06-20T12:00:05Z",
          "endedAgeMs": 250
        }
      },
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

Authenticated JSON containing host CPU, host memory, disk, host-wide network
rates, and an `engine` object for restream self metrics. `engine.cpuPercent`
and `engine.totalMemoryBytes` include the restream process plus child FFmpeg
processes launched by restream; `restream*` and `externalFfmpeg*` fields provide
the breakdown. This is not Prometheus text format.
Query params:

| Param | Default | Notes |
| --- | --- | --- |
| `view` | `full` | `summary` trims steady-state dashboard polls down to aggregate percentages/rates plus engine totals. The first dashboard load still uses `full` so static disk/interface metadata can be cached client-side. |

### `GET /api/v1/engine`

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
      "license": "MPL-2.0",
      "bondingAvailable": true
    },
    "mbedtls": {
      "version": "Mbed TLS 3.6.6",
      "buildVersion": "3.6.6",
      "license": "Apache-2.0"
    },
    "sqlite": { "version": "3.x", "sourceId": "...", "license": "blessing" },
    "x264": {
      "version": "0.164.x",
      "license": "GPL-2.0-or-later",
      "versionSource": "linked pkg-config metadata at build time"
    },
    "x265": {
      "version": "3.x",
      "license": "GPL-2.0-or-later",
      "versionSource": "linked pkg-config metadata at build time"
    }
  },
  "sbom": {
    "format": "CycloneDX",
    "specVersion": "1.5",
    "endpoint": "/api/v1/engine/sbom",
    "componentCount": 100,
    "rustComponentCount": 85,
    "nativeComponentCount": 16,
    "nativeComponents": ["libavcodec", "..."],
    "licensesIncluded": true
  },
  "os": {
    "platform": "linux",
    "arch": "x86_64",
    "hostname": "host",
    "kernelVersion": "6.x",
    "uptime": 12345,
    "totalMem": 17179869184,
    "cpu": {
      "modelName": "13th Gen Intel(R) Core(TM) i9-13900H",
      "logicalCpus": 20,
      "physicalCores": 10,
      "threadsPerCore": 2.0,
      "virtualization": "VT-x",
      "hypervisorDetected": true,
      "hypervisorVendor": "Microsoft",
      "flags": ["sse4_1", "sse4_2", "avx", "avx2", "fma", "aes", "vmx", "hypervisor"]
    }
  }
}
```

For native libraries that expose both `version` and `buildVersion`, `version` is
queried from the library loaded by the running process and `buildVersion` is the
version resolved by the build script at compile time. They should normally
match; a mismatch is a packaging/linking diagnostic.

Native versions are obtained from the running libraries where they expose a
runtime API. x264 and x265 have no public runtime version call, so their exact
linked pkg-config versions are embedded at build time and labeled accordingly.

The `os.cpu` object is intentionally a production-debug subset rather than an
`lscpu` clone. It identifies the CPU model, core/thread topology, virtualization
context, and acceleration features that can explain codec throughput, WSL/cloud
behavior, and native-library performance differences.

### `GET /api/v1/engine/sbom`

Authenticated CycloneDX 1.5 JSON software bill of materials. The response uses
content type `application/vnd.cyclonedx+json; version=1.5` and contains:

- the Restream application component and build identity;
- every resolved normal/runtime Rust crate from Cargo's locked dependency
  graph, including version, Cargo package URL, source, and declared license;
- FFmpeg component libraries, SRT, libmbedcrypto, SQLite, x264, x265, glibc
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

### `GET /api/v1/engine`

Authenticated canonical engine/runtime status envelope for the frontend control
plane.

### `GET /api/v1/engine/health`

Authenticated engine health snapshot. Returns the same pipeline/ingest/output
health model documented above on the v1 authenticated control-plane surface.

Query params:

| Param | Default | Notes |
| --- | --- | --- |
| `view` | `full` | `summary` trims steady-state overview/control polls down to per-pipeline status, bitrate, uptime, recording state, and reconnect/grace flags. Pipeline, inspect, and publisher-quality flows continue using `full`. |

Summary response shape:

```json
{
  "status": "ready",
  "pipelines": {
    "pipeline_id": {
      "input": {
        "status": "on",
        "publishStartedAt": "2026-06-20T11:59:00Z",
        "probeReady": true,
        "probeStatus": "ready",
        "probePendingMs": null,
        "bytesReceived": 12000000,
        "bytesSent": 24000000,
        "readers": 2,
        "bitrateKbps": 1600,
        "publisher": {
          "protocol": "srt",
          "remoteAddr": "203.0.113.10:50000"
        },
        "disconnectGraceActive": false,
        "disconnectGraceRemainingMs": null
      },
      "outputs": {
        "output_id": {
          "status": "running",
          "uptimeSecs": 42.5,
          "totalSize": 16000000,
          "bitrateKbps": 1500,
          "retrying": false
        }
      },
      "recording": { "enabled": false, "active": false }
    }
  }
}
```

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

### `GET /api/v1/pipelines`

Authenticated pipeline list with ingest URLs and configured pipeline metadata.

### `GET /api/v1/pipelines/:pipelineId`

Authenticated pipeline detail endpoint. Returns one pipeline plus its
configured outputs. Returns 404 for unknown pipeline IDs.

### `GET /api/v1/pipelines/:pipelineId/alerts`

Alerts for a single pipeline. Same alert shape as the aggregate endpoint.

### `GET /api/v1/pipelines/:pipelineId/graph`

Authenticated pipeline graph endpoint. Returns 404 for unknown pipeline IDs.

### `GET /api/v1/pipelines/:pipelineId/diagnostics`

Authenticated SSE diagnostics endpoint for the pipeline diagnostics stream.

### `GET /api/v1/settings`

Authenticated settings/configuration read endpoint.
Supports `?jobs=latest` for consumers that only need the newest job per output,
and `?view=dashboard` for the slim dashboard config shape used by runtime
overview/control flows.

### `PATCH /api/v1/settings`

Authenticated settings/configuration update endpoint.

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
