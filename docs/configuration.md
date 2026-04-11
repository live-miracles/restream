# Configuration Reference

Restream configuration uses two layers with precedence:

`ENV > src/config/restream.json > defaults`

## 1. Environment Variables

### Server

| Variable | Default | Description |
|---|---|---|
| `PORT` | `3030` | Express listen port |
| `LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` |

### MediaMTX Internal (backend to MediaMTX)

| Variable | Default | Description |
|---|---|---|
| `MEDIAMTX_INTERNAL_HOST` | `localhost` | Hostname/IP used by backend for MediaMTX API and RTSP pull |
| `MEDIAMTX_INTERNAL_API_PORT` | `9997` | MediaMTX API port |
| `MEDIAMTX_INTERNAL_RTSP_PORT` | `8554` | MediaMTX RTSP port used by FFmpeg/ffprobe pull URLs |

### MediaMTX Ingest (publisher-facing URLs in UI)

| Variable | Default | Description |
|---|---|---|
| `MEDIAMTX_INGEST_HOST` | dashboard host | Hostname/IP shown to publishers |
| `MEDIAMTX_INGEST_RTMP_PORT` | `1935` | RTMP ingest port |
| `MEDIAMTX_INGEST_RTSP_PORT` | `8554` | RTSP ingest port |
| `MEDIAMTX_INGEST_SRT_PORT` | `8890` | SRT ingest port |

### FFmpeg and ffprobe

| Variable | Default | Description |
|---|---|---|
| `FFMPEG_PATH` | `ffmpeg` | FFmpeg executable |
| `FFPROBE_PATH` | `ffprobe` | ffprobe executable |

### App Config Path

| Variable | Default | Description |
|---|---|---|
| `RESTREAM_CONFIG_PATH` | `src/config/restream.json` | Path to app JSON config |

### Probe Cache

| Variable | Default | Description |
|---|---|---|
| `PROBE_CACHE_TTL_MS` | `30000` | ffprobe cache TTL in ms |

## 2. Application Config File

File: `src/config/restream.json`

```json
{
  "server-name": "Server Name",
  "pipelines-limit": 25,
  "out-limit": 95,
  "mediamtx": {
    "internal": {
      "host": "localhost",
      "apiPort": "9997",
      "rtspPort": "8554"
    },
    "ingest": {
      "host": "stream.example.com",
      "rtmpPort": "1935",
      "rtspPort": "8554",
      "srtPort": "8890"
    }
  }
}
```

### Config Keys

| Key | Type | Default | Notes |
|---|---|---|---|
| `server-name` | string | `Server Name` | Display name in UI |
| `pipelines-limit` | integer | `25` | Positive integer |
| `out-limit` | integer | `95` | Positive integer |
| `mediamtx.internal.host` | string | `localhost` | Backend internal host |
| `mediamtx.internal.apiPort` | string | `9997` | Backend MediaMTX API port |
| `mediamtx.internal.rtspPort` | string | `8554` | Backend MediaMTX RTSP pull port |
| `mediamtx.ingest.host` | string | `null` | Publisher-facing host. If omitted, UI uses dashboard hostname |
| `mediamtx.ingest.rtmpPort` | string | `1935` | Publisher RTMP ingest port |
| `mediamtx.ingest.rtspPort` | string | `8554` | Publisher RTSP ingest port |
| `mediamtx.ingest.srtPort` | string | `8890` | Publisher SRT ingest port |

UI ingest URLs are built in app code as:

- RTMP: `rtmp://<ingest.host>:<ingest.rtmpPort>/<streamKey>`
- RTSP: `rtsp://<ingest.host>:<ingest.rtspPort>/<streamKey>`
- SRT: `srt://<ingest.host>:<ingest.srtPort>?streamid=publish:<streamKey>`

Backend internal URLs are built as:

- API: `http://<internal.host>:<internal.apiPort>`
- RTSP base: `rtsp://<internal.host>:<internal.rtspPort>`

## 3. docker-compose Environment Example

`app` service environment:

```yaml
environment:
  NODE_ENV: production
  PORT: 3030
  MEDIAMTX_INTERNAL_HOST: mediamtx
```

`MEDIAMTX_INTERNAL_API_PORT` and `MEDIAMTX_INTERNAL_RTSP_PORT` can be omitted in compose when MediaMTX uses defaults (`9997` and `8554`).

Optional publisher-facing overrides:

```yaml
environment:
  MEDIAMTX_INGEST_HOST: your-public-host
  MEDIAMTX_INGEST_RTMP_PORT: '1935'
  MEDIAMTX_INGEST_RTSP_PORT: '8554'
  MEDIAMTX_INGEST_SRT_PORT: '8890'
```

## 4. MediaMTX Ports

| Port | Protocol | Purpose |
|---|---|---|
| `1935` | RTMP | RTMP ingest |
| `8554` | RTSP | RTSP ingest/relay |
| `8890` | SRT | SRT ingest |
| `9997` | HTTP | MediaMTX API |
| `8888` | HTTP | HLS / HTTP interface |
| `8889` | HTTP/WS | WebRTC signaling |
| `8189` | TCP/UDP | WebRTC media |
