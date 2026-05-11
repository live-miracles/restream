# Configuration Reference

Restream is configured through environment variables. There is no JSON config file.

Server name and custom encodings are managed at runtime via the Settings page (`/settings.html`) and stored in SQLite.

## 1. Environment Variables

### Server

| Variable | Default | Description |
|---|---|---|
| `PORT` | `3030` | Express listen port |
| `LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` |

### MediaMTX Backend (hardcoded localhost)

The backend assumes MediaMTX is always available on `localhost` with default ports:
- API: `http://localhost:9997`
- RTMP: `rtmp://localhost:1935`
- SRT: `srt://localhost:8890`
- HLS: `http://localhost:8888`

These are hardcoded and cannot be overridden via environment variables.

Publisher-facing ingest URLs shown in the dashboard are rewritten by the frontend to use the browser's current hostname, so they automatically reflect the correct public address without any configuration.

### FFmpeg and ffprobe

| Variable | Default | Description |
|---|---|---|
| `FFMPEG_PATH` | `ffmpeg` | FFmpeg executable |
| `FFPROBE_PATH` | `ffprobe` | ffprobe executable |

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
- `failureCount` counts failures regardless of whether input was on or off at the time. After 100 total failures, the output gives up.
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

## 2. Runtime Settings (UI)

Server name and custom encodings are stored in the SQLite `meta` and `encodings` tables and managed through the Settings page at `/settings.html`.

| Setting | Default | Description |
|---|---|---|
| Server name | `Restream` | Display name shown in the dashboard navbar |
| Encodings | — | Custom FFmpeg encoding presets; see [API Reference](./api-reference.md) for the `/encodings` endpoints |

## 3. Local Host Run

For long-lived systemd deployment on a Linux host, see [deployment-host.md](./deployment-host.md).

MediaMTX and the Node.js app run as host processes.

```sh
npm ci
./mediamtx     # or mediamtx.exe on Windows
npm start      # run in a second terminal
```

For development mode:

```sh
npm run dev
npm run css-watch
```

This runs the app with `npm run dev` (nodemon) instead of `node src/index.js`.

If you encounter port conflicts, check for running MediaMTX or Node processes and stop them manually.

## 4. MediaMTX Ports

| Port | Protocol | Purpose |
|---|---|---|
| `1935` | RTMP | RTMP ingest and internal FFmpeg pull |
| `8890` | SRT | SRT ingest and internal FFmpeg pull/probe |
| `9997` | HTTP | MediaMTX API |
| `8888` | HTTP | HLS preview interface (localhost-only) |

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
