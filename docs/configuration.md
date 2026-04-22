# Configuration Reference

Restream configuration uses two layers with precedence:

`ENV > src/config/restream.json > defaults`

## 1. Environment Variables

### Server

| Variable | Default | Description |
|---|---|---|
| `PORT` | `3030` | Express listen port |
| `LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` |

### MediaMTX Backend (hardcoded localhost)

The backend assumes MediaMTX is always available on `localhost` with default ports:
- API: `http://localhost:9997`
- RTSP: `rtsp://localhost:8554`
- HLS: `http://localhost:8888`

These are hardcoded in the application and cannot be overridden via environment variables.

### MediaMTX Ingest (publisher-facing URLs in UI)

| Variable | Default | Description |
|---|---|---|
| `MEDIAMTX_INGEST_HOST` | dashboard host | Hostname/IP shown to publishers |

Protocol ports are read from MediaMTX runtime config via `GET /v3/config/global/get`.
If port discovery fails, backend leaves the corresponding `ingestUrls` field as `null`.

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

### Output Recovery

| Variable | Default | Description |
|---|---|---|
| `OUTPUT_RECOVERY_ENABLED` | `true` | Enable automatic output restart and input recovery restart logic |
| `OUTPUT_RECOVERY_IMMEDIATE_RETRIES` | `3` | Number of fixed-delay retries before exponential backoff |
| `OUTPUT_RECOVERY_IMMEDIATE_DELAY_MS` | `1000` | Delay between fixed-delay retries |
| `OUTPUT_RECOVERY_BACKOFF_RETRIES` | `5` | Number of exponential-backoff retries after immediate retries |
| `OUTPUT_RECOVERY_BACKOFF_BASE_DELAY_MS` | `2000` | Initial backoff delay |
| `OUTPUT_RECOVERY_BACKOFF_MAX_DELAY_MS` | `60000` | Maximum backoff delay cap |
| `OUTPUT_RECOVERY_RESET_FAILURE_COUNT_AFTER_MS` | `30000` | Reset failure streak if an output ran at least this long before failing |
| `OUTPUT_RECOVERY_RESTART_ON_INPUT_RECOVERY` | `true` | Enable output restart scheduling when input transitions back to `on` |
| `OUTPUT_RECOVERY_INPUT_RECOVERY_RESTART_MODE` | `inputUnavailableOnly` | `inputUnavailableOnly` restarts only outputs likely stopped by input unavailability; `failedOnly` restarts failed outputs plus input-unavailable stops; `all` restarts every output |
| `OUTPUT_RECOVERY_INPUT_RECOVERY_RESTART_DELAY_MS` | `1000` | Delay before the first output restart on input recovery |
| `OUTPUT_RECOVERY_INPUT_RECOVERY_RESTART_STAGGER_MS` | `250` | Per-output stagger when restarting outputs on input recovery |

### Output Recovery Semantics

- Total retry budget per failure streak is `immediateRetries + backoffRetries`.
- `failureCount` counts failures, not retries. The first failed run is `failureCount=1`.
- Retry is scheduled only while `failureCount <= totalRetries`.
- Output control is intent-driven: each output persists `desiredState` as either `running` or `stopped`.
- Manual stop sets `desiredState=stopped`, clears pending retry timers, and suppresses retry and input-recovery restarts until a later start sets `desiredState=running` again.
- Newly created outputs default to `desiredState=stopped`; they do not auto-start until explicitly started.
- Unrequested terminal exits retry while `desiredState=running`, including clean `exitCode=0` exits, except when the exit is correlated to a recent `on -> non-on` input transition.
- When the next failure exceeds the retry budget, the job logs:
  - `[lifecycle] retry_decision failureCount=<n> scheduled=false`
  - `[lifecycle] retry_exhausted failureCount=<n> totalRetries=<m> action=give_up`
- Maximum wait time for one contiguous failure streak is bounded by configured delays.
  - With defaults: `3 * 1000ms + (2000 + 4000 + 8000 + 16000 + 32000) = 65000ms` total delayed wait.
- `OUTPUT_RECOVERY_RESET_FAILURE_COUNT_AFTER_MS` resets the failure streak when a run survives at least that duration before failing.
  - This means retries can continue across long runtimes instead of permanently exhausting after one early burst.
- `inputUnavailableOnly` and `failedOnly` input-recovery selection correlate output exits with the most recent `on -> non-on` input transition using a built-in grace window of `max(3 * HEALTH_SNAPSHOT_INTERVAL_MS, 15000ms)`.
- Retry and input-recovery timers only attempt starts for outputs whose `desiredState` is still `running`.
- `outputRecovery` in `src/config/restream.json` is optional. If omitted, built-in defaults are still active and env overrides still apply.

## 2. Application Config File

File: `src/config/restream.json`

`outputRecovery` is optional in this file; this example shows all available fields.

```json
{
  "serverName": "Server Name",
  "pipelinesLimit": 25,
  "outLimit": 95,
  "outputRecovery": {
    "enabled": true,
    "immediateRetries": 3,
    "immediateDelayMs": 1000,
    "backoffRetries": 5,
    "backoffBaseDelayMs": 2000,
    "backoffMaxDelayMs": 60000,
    "resetFailureCountAfterMs": 30000,
    "restartOnInputRecovery": true,
    "inputRecoveryRestartMode": "inputUnavailableOnly",
    "inputRecoveryRestartDelayMs": 1000,
    "inputRecoveryRestartStaggerMs": 250
  },
  "mediamtx": {
    "ingest": {
      "host": "stream.example.com"
    }
  }
}
```

### Config Keys

| Key | Type | Default | Notes |
|---|---|---|---|
| `host` | string | `0.0.0.0` | Express bind host. Overridden by `HOST` env when set |
| `serverName` | string | `Server Name` | Display name in UI |
| `pipelinesLimit` | integer | `25` | Positive integer |
| `outLimit` | integer | `95` | Positive integer |
| `outputRecovery.enabled` | boolean | `true` | Master switch for output auto-restart logic |
| `outputRecovery.immediateRetries` | integer | `3` | Fixed-delay retry attempts after eligible unexpected output exits |
| `outputRecovery.immediateDelayMs` | integer | `1000` | Delay for fixed-delay retries |
| `outputRecovery.backoffRetries` | integer | `5` | Exponential-backoff retry attempts after immediate retries |
| `outputRecovery.backoffBaseDelayMs` | integer | `2000` | Base delay for exponential backoff |
| `outputRecovery.backoffMaxDelayMs` | integer | `60000` | Maximum backoff delay |
| `outputRecovery.resetFailureCountAfterMs` | integer | `30000` | Failure streak reset threshold for long-running jobs |
| `outputRecovery.restartOnInputRecovery` | boolean | `true` | Enable restart scheduling when pipeline input comes back |
| `outputRecovery.inputRecoveryRestartMode` | string | `inputUnavailableOnly` | Recovery restart mode: `inputUnavailableOnly`, `failedOnly`, or `all` |
| `outputRecovery.inputRecoveryRestartDelayMs` | integer | `1000` | Initial delay before recovery restart |
| `outputRecovery.inputRecoveryRestartStaggerMs` | integer | `250` | Stagger between output restarts during recovery |
| `mediamtx.ingest.host` | string | `null` | Publisher-facing host. If omitted, UI uses dashboard hostname |

Ingest URLs are precomputed by backend helpers and returned on stream-key and pipeline payloads.
The effective MediaMTX path is always `live/<streamKey>`.

- RTMP: `rtmp://<ingest.host>:<rtmpPort>/live/<streamKey>`
- RTSP: `rtsp://<ingest.host>:<rtspPort>/live/<streamKey>`
- SRT: `srt://<ingest.host>:<srtPort>?streamid=publish:live/<streamKey>`

`rtmpPort`, `rtspPort`, and `srtPort` are derived from MediaMTX global runtime config (`/v3/config/global/get`).

`GET /config` exposes only `ingestHost` in the public config payload. Ports are not app-configurable and are resolved from MediaMTX at runtime for server-side `ingestUrls` generation.

Backend internal URLs are hardcoded as:

- API: `http://localhost:9997`
- RTSP base: `rtsp://localhost:8554`

## 3. docker-compose Profiles

The project uses one compose file with two profiles:

- `host` profile: runs `mediamtx` in Docker for host-mode development.
- `container` profile: runs `pause`, `app`, and `mediamtx-pod` for full Docker mode.

`nginx-rtmp` has no profile and is available in both modes.

### Host mode (`make run-host`)

Starts `mediamtx` + `nginx-rtmp` in Docker and runs Node on host.

```sh
docker compose --profile host up -d mediamtx nginx-rtmp
```

The host profile runs MediaMTX with `network_mode: host`. The container shares the host's network
stack directly, so MediaMTX's default loopback bindings (`apiAddress: 127.0.0.1:9997`,
`hlsAddress: 127.0.0.1:8888` from `infra/mediamtx.yml`) are accessible on the host's own localhost
without any env overrides or port mappings.

If host networking is unavailable (e.g. Docker Desktop on macOS/Windows), disable/remove
`network_mode: host` and uncomment the `environment` and `ports` blocks in
`docker-compose.yml` to revert to the bridge-networking fallback, which overrides
`MTX_APIADDRESS=0.0.0.0:9997` and `MTX_HLSADDRESS=0.0.0.0:8888` and exposes them via
`127.0.0.1:9997:9997` and `127.0.0.1:8888:8888`.

### Container mode (`make run-docker`)

Starts app + MediaMTX in a shared namespace through `pause`.

```sh
docker compose --profile container up -d --build --force-recreate --renew-anon-volumes pause mediamtx-pod nginx-rtmp app
```

### app container environment

`app` service environment:

```yaml
environment:
  NODE_ENV: production
  PORT: 3030
```

The app automatically connects to MediaMTX on `localhost:9997` (API) and `localhost:8554` (RTSP).

Optional publisher-facing overrides:

```yaml
environment:
  MEDIAMTX_INGEST_HOST: your-public-host
```

## 4. MediaMTX Ports

| Port | Protocol | Purpose |
|---|---|---|
| `1935` | RTMP | RTMP ingest |
| `8554` | RTSP | RTSP ingest/relay |
| `8890` | SRT | SRT ingest |
| `9997` | HTTP | MediaMTX API |
| `8888` | HTTP | HLS / HTTP interface |

## 5. Input Preview Proxy

Dashboard input preview uses an app-level HLS proxy endpoint instead of sending browser traffic directly
to MediaMTX.

- Browser URL shape: `/preview/hls/<streamKey>/index.m3u8`
- App proxy upstream: `http://localhost:8888/live/<streamKey>/...`

Why this exists:

- keep browser requests on the same origin as the dashboard
- avoid exposing MediaMTX HLS directly to remote clients for preview
- centralize preview policy and error handling in the app

Notes:

- The proxy validates stream keys and asset paths before forwarding.
  Stream keys allow alphanumeric, `_`, `.`, and `-`, with `.` and `..` explicitly rejected.
- The dashboard preview now uses the normal proxied HLS master manifest unchanged.
- The backend no longer rewrites preview manifests; `.m3u8` requests are forwarded as plain
  pass-through proxy responses after validation.
- Current Chromium plus bundled `hls.js` playback works against that unchanged master manifest in
  this repository's preview flow.
- MediaMTX is configured for lower idle resource usage with `hlsAlwaysRemux: no` and
  `hlsVariant: mpegts`.
- This avoids maintaining active HLS muxers for all ready paths and reduces steady CPU/RAM use.
- The tradeoff is slower first-preview startup because muxers are created on demand when a viewer
  clicks Play.
- In HTTPS deployments, terminate TLS on the dashboard origin and keep preview requests
  same-origin so browsers do not hit mixed-content blocks.
- The dashboard preview player is lazy-loaded: selecting a pipeline does not request HLS
  assets until the user presses Play.
