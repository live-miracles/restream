# Configuration Reference

Restream uses environment variables for process-level settings and the Settings page for runtime app settings. There is no JSON config file.

Server name, dashboard password, ingest security, and custom encodings are managed at runtime via
the Settings page (`/settings.html`) and stored in SQLite.

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

Set **Ingest Host** on the Settings page to the public hostname or IP address publishers should use.
The value is persisted in SQLite and returned as `ingestHost` by `GET /config` and `PATCH /config`.
Generated URLs from `/config` and `/stream-keys` use this value, or `localhost` when it is blank.

For Cloudflare Tunnel deployments, set this to the direct ingest DNS name or VM IP. The tunnel only
carries the HTTPS dashboard/API traffic; RTMP and SRT publishers still connect directly to the
configured ingest host.

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

Server name, dashboard password, ingest security, and custom encodings are stored in SQLite and
managed through the Settings page at `/settings.html`.

| Setting | Default | Description |
|---|---|---|
| Server name | `Restream` | Display name shown in the dashboard navbar |
| Dashboard password | `admin` on first run | Password for the dashboard/API session cookie. Change it after first login. |
| Ingest security failure limit | `10` | Failed publish attempts from one IP before a temporary ban |
| Ingest security failure window | `60000` ms | Rolling window for failed publish attempts |
| Ingest security ban duration | `600000` ms | Temporary ban duration after the failure limit is reached |
| Ingest security tracked IP limit | `10000` | Maximum number of IP records kept in memory for ingest security |
| Encodings | — | Custom FFmpeg encoding presets; see [API Reference](./api-reference.md) for the `/encodings` endpoints |

MediaMTX delegates publish/read/playback authorization to the local Restream endpoint
`/internal/mediamtx/auth`. Publish attempts must target a configured `live/<streamKey>` path.
Repeated unknown-key attempts from the same publisher IP are temporarily banned.

The dashboard, API, Grafana proxy, preview, and media file routes require a valid dashboard session.
`/login`, `/healthz`, and `/internal/mediamtx/auth` are intentionally left unauthenticated so users
can sign in, health checks can run, and MediaMTX can call its local auth callback. To reset a
forgotten dashboard password on the Linux VM, run:

```sh
sudo bash /opt/restream/scripts/server-reset-password.sh
```

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

Use `monitoring/prometheus.yml` as the tracked Prometheus scrape config copied by the server
scripts, and see
[Observability](./observability.md) for the Prometheus/Grafana setup.

Grafana can be reached through the Node proxy at `/grafana/` when the local Grafana service is
running. Keep Grafana's own `3000` listener localhost-only.

## 6. HLS Pull

The engine serves its in-memory HLS package directly:

```text
/hls/<pipelineId>/index.m3u8
/hls/<pipelineId>/seg<N>.ts
```

`/hls/<pipelineId>` is a shorter playlist alias. Existing
`/preview/hls/<pipelineId>/...` URLs remain available as deprecated
compatibility aliases.

The dashboard and external pull clients use the same `HlsStore`; opening a
playlist does not create a second muxer or copy the segment data.

### Future Authorization TODO

HLS pull routes are currently unauthenticated. Before exposing them outside a
trusted network, add authorization that works for both playlists and segments:

- signed URLs or short-lived bearer tokens scoped to a pipeline;
- expiry validation with a small clock-skew allowance;
- the same authorization scope propagated to every segment URL;
- optional audience/client binding where platform behavior permits it;
- key rotation and immediate revocation for compromised links;
- rate limits and concurrent-reader limits per pipeline/token;
- explicit CORS, cache-control, and CDN policy;
- audit logs that avoid recording full reusable tokens;
- tests for expired, altered, revoked, cross-pipeline, and missing tokens.

Keep authorization optional for localhost test sinks and configurable for
private deployments.
