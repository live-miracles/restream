# Restream API Reference

Base URL: `http://localhost:3030`

JSON uses camelCase. Unless noted otherwise, routes require the `session` cookie
returned by login.

## Authentication

| Method | Route | Purpose |
|---|---|---|
| `POST` | `/api/auth/login` | Create a persisted session from `{ "password": "..." }` |
| `POST` | `/api/auth/logout` | Delete the current session |
| `POST` | `/api/auth/change-password` | Change the password; existing sessions remain valid |

Static pages/assets are served without an auth gate; protected API handlers
enforce the cookie themselves. `/health`, `/healthz`, HLS pull routes, and
`/audio-caps` are also public.

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

Deletion cancels configured output tasks and the active ingest before removing
the pipeline row. Shared transcoder, HLS, and recording cleanup still follows
their existing task lifecycle.

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

Deleting a running output cancels and unregisters its active egress before the
database row is removed.

URL behavior:

| URL prefix | Egress |
|---|---|
| `rtmp://` | RTMP |
| `srt://` | SRT/MPEG-TS |
| `hls://` | Local in-memory HLS segmenter |
| `http://` | Local in-memory HLS segmenter |
| `https://` | Local in-memory HLS segmenter |

Any other prefix is rejected during validation with a `400 Bad Request`. RTMPS is not supported and will be rejected. HLS upload via HTTP/HTTPS is not implemented; the target URL is only used for scheme identification.

## History

| Method | Route | Response |
|---|---|---|
| `GET` | `/pipelines/:pipelineId/history` | `{ pipelineId, logs }` |
| `GET` | `/pipelines/:pipelineId/outputs/:outputId/history` | `{ pipelineId, outputId, logs }` |

The current handlers do not expose query filtering even though the DB layer has
filter support.

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

The recording path intends to write Matroska files in `media/`, and files whose
task lifetime is shorter than five seconds are deleted as transient artifacts.
The current feeder concatenates raw packet payloads into FFmpeg input detection,
so readable recording output is not yet guaranteed.

## File Ingest

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/ingests` | List configured file ingests |
| `POST` | `/api/ingests` | Create |
| `PUT` | `/api/ingests/:id` | Update |
| `DELETE` | `/api/ingests/:id` | Delete |
| `POST` | `/api/ingests/:id/start` | Spawn system FFmpeg into local RTMP |
| `POST` | `/api/ingests/:id/stop` | Kill the tracked child |

Create/update body:

```json
{
  "filename": "example.mp4",
  "streamKey": "stream-key",
  "loop": true,
  "startTime": "00:00:05"
}
```

Start returns `400` if `media/<filename>` does not exist and `409` if that ingest
ID already has a tracked child. The list endpoint currently reports
`running: false` for every row; it does not consult the child map. Natural child
exit is not reaped from the map, so a later start can remain stuck at `409`.
Deleting an ingest definition terminates its running child process if one exists.

## Media Files

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/media` | List `.mkv`, `.mp4`, and `.mov` files in `media/` |
| `DELETE` | `/api/media/:filename` | Delete an unreferenced file |

Deletion returns `409` when a configured file ingest references the filename.

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
      "version": "6.1.5",
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
    }
  },
  "sbom": {
    "format": "CycloneDX",
    "specVersion": "1.5",
    "endpoint": "/api/status/sbom",
    "componentCount": 100,
    "rustComponentCount": 85,
    "nativeComponentCount": 15,
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
runtime API. x264 has no public runtime version call, so its exact linked
pkg-config version is embedded at build time and labeled accordingly.

### `GET /api/status/sbom`

Authenticated CycloneDX 1.5 JSON software bill of materials. The response uses
content type `application/vnd.cyclonedx+json; version=1.5` and contains:

- the Restream application component and build identity;
- every resolved normal/runtime Rust crate from Cargo's locked dependency
  graph, including version, Cargo package URL, source, and declared license;
- FFmpeg component libraries, SRT, libssl, libcrypto, SQLite, x264, glibc when
  applicable, Rust's standard library, libstdc++, and libgcc;
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

These routes are currently unauthenticated.
