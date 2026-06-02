# Configuration Reference

Restream uses environment variables for process-level settings and the Settings page for runtime app settings. There is no JSON config file.

Server name, ingest security, and custom encodings are managed at runtime via the Settings page (`/settings.html`) and stored in SQLite.

## 1. Environment Variables

### Server

| Variable | Default | Description |
|---|---|---|
| `LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` |
| `BASE_PATH` | empty | Optional URL path prefix for serving the dashboard/API behind a path-based proxy, for example `/media-mtx-test`. |
| `PUBLIC_INGEST_HOST` | empty | Optional public hostname or IP override for RTMP/SRT ingest URLs. Leave empty on GCP when the VM external IP should be discovered from metadata. |
| `PUBLIC_INGEST_METADATA_TIMEOUT_MS` | `1000` | Timeout for reading the VM external IP from the GCE metadata server. |

The Express app always listens on port `3030` so MediaMTX can use the fixed local auth callback at `http://127.0.0.1:3030/internal/mediamtx/auth`.

When `BASE_PATH` is set, the app accepts dashboard and API requests under that prefix while
keeping the internal route definitions unchanged. For example:

| Instance | `BASE_PATH` | Cloudflare public URL |
|---|---|---|
| `media-mtx-test` | `/media-mtx-test` | `https://livestream.example.com/media-mtx-test/` |
| `media-mtx-test-v1` | `/media-mtx-test-v1` | `https://livestream.example.com/media-mtx-test-v1/` |

Cloudflare should route each path prefix to the matching VM tunnel. The prefix is for HTTPS
dashboard/API traffic only; publishers still use the VM's RTMP/SRT ingest ports directly.

### MediaMTX Backend (hardcoded localhost)

The backend assumes MediaMTX is always available on `localhost` with default ports:
- API: `http://localhost:9997`
- RTMP: `rtmp://localhost:1935`
- SRT: `srt://localhost:8890`
- HLS: `http://localhost:8888`
- Metrics: `http://localhost:9998`

These are hardcoded and cannot be overridden via environment variables.

Publisher-facing ingest URLs are based on the MediaMTX RTMP/SRT ports. The backend returns
`PUBLIC_INGEST_HOST` when it is set; otherwise it returns `localhost`. The dashboard also calls
`/api/public-ingest`, which resolves the public ingest host from `PUBLIC_INGEST_HOST` or, on GCP,
from the VM metadata server's external IP endpoint. Outside GCP, it falls back to the first
non-loopback local IPv4 address. When a host is available, the dashboard uses it to display RTMP/SRT
URLs.

For Cloudflare Tunnel deployments, leave `PUBLIC_INGEST_HOST` empty on GCP unless a custom ingest
DNS name is needed. The tunnel only carries the HTTPS dashboard/API traffic; RTMP and SRT
publishers still connect directly to MediaMTX on the VM's ingest ports.

### Grafana Proxy

| Variable | Default | Description |
|---|---|---|
| `GRAFANA_PROXY_PATH` | `/grafana` | Dashboard path that proxies browser requests to Grafana |
| `GRAFANA_PROXY_TARGET` | `http://127.0.0.1:3000` | Local Grafana upstream for the Node proxy |
| `GRAFANA_PROXY_TOKEN` | empty | Optional shared token. When set, requests need `Authorization: Bearer <token>` or a one-time `?grafana_token=<token>` cookie bootstrap. |
| `GRAFANA_PROXY_TIMEOUT_MS` | `30000` | Timeout for upstream Grafana proxy requests |

Grafana itself should stay bound to localhost. Public users should reach it through the Restream
dashboard origin at `/grafana/`.

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

Server name, ingest security, and custom encodings are stored in SQLite and managed through the Settings page at `/settings.html`.

| Setting | Default | Description |
|---|---|---|
| Server name | `Restream` | Display name shown in the dashboard navbar |
| Ingest security failure limit | `10` | Failed publish attempts from one IP before a temporary ban |
| Ingest security failure window | `60000` ms | Rolling window for failed publish attempts |
| Ingest security ban duration | `600000` ms | Temporary ban duration after the failure limit is reached |
| Ingest security tracked IP limit | `10000` | Maximum number of IP records kept in memory for ingest security |
| Encodings | — | Custom FFmpeg encoding presets; see [API Reference](./api-reference.md) for the `/encodings` endpoints |

MediaMTX delegates publish/read/playback authorization to the local Restream endpoint
`/internal/mediamtx/auth`. Publish attempts must target a configured `live/<streamKey>` path.
Repeated unknown-key attempts from the same publisher IP are temporarily banned.

## 3. Local Host Run

For long-lived systemd deployment on a Linux host, see the [Linux VM Deployment](../README.md#linux-vm-deployment-gcp) section in the README.

MediaMTX and the Node.js app run as host processes.

```sh
npm ci
./mediamtx     # or mediamtx.exe on Windows
npm start      # run in a second terminal
```

For development mode, run each in its own terminal:

```sh
npm run dev             # backend with live reload via tsx watch
npm run watch:frontend  # frontend TypeScript in watch mode
npm run css-watch       # Tailwind CSS in watch mode
```

`npm run dev` runs the backend TypeScript source directly via `tsx` — no compile step needed.

If you encounter port conflicts, check for running MediaMTX or Node processes and stop them manually.

## 4. MediaMTX Ports

| Port | Protocol | Purpose |
|---|---|---|
| `1935` | RTMP | RTMP ingest and internal FFmpeg pull |
| `8890` | SRT | SRT ingest and internal FFmpeg probe |
| `9997` | HTTP | MediaMTX API |
| `9998` | HTTP | MediaMTX Prometheus metrics |
| `8888` | HTTP | HLS preview interface (localhost-only) |

MediaMTX publish/read/playback authorization calls the Node app on
`http://127.0.0.1:3030/internal/mediamtx/auth`. Do not expose `/internal/*` through a public
reverse proxy. Keep MediaMTX API, metrics, HLS, and the auth callback on localhost-only bindings.

## 5. Observability

MediaMTX Prometheus-compatible metrics are enabled on `127.0.0.1:9998`.

```sh
curl -fsS http://127.0.0.1:9998/metrics | head
```

Use `monitoring/prometheus.yml` as a starter Prometheus scrape config, and see
[Observability](./observability.md) for the Prometheus/Grafana setup.

Grafana can be reached through the Node proxy at `/grafana/` when a local Grafana instance is
running. Keep Grafana's own `3000` listener localhost-only.

## 6. Input Preview Proxy

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
