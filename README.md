# Restream

Restream is a host-run streaming control plane built on [MediaMTX](https://github.com/bluenviron/mediamtx). It manages stream keys, pipelines, output destinations, and FFmpeg jobs from a browser dashboard.

## How It Works

1. A publisher sends RTMP or SRT ingest to MediaMTX, or a pre-recorded video in `/media` is used as the input source.
2. Restream stores stream keys, pipelines, outputs, job state, and logs in local SQLite (`data.db`).
3. When an output starts, Restream probes the MediaMTX path, spawns FFmpeg pulling from MediaMTX, and tracks the process.
4. The dashboard reads `/config` and `/health` to show pipeline state, output state, system metrics, and logs.

MediaMTX owns media routing. Restream owns orchestration and state.

## Features

- **Pipeline management** — create pipelines tied to stream keys; start/stop outputs per pipeline
- **Output encoding** — source copy, 720p, 1080p, vertical-crop, vertical-rotate, and custom FFmpeg presets
- **Pipeline recording** — record any live pipeline to an MP4 file in the `/media` folder
- **Video ingest** — use pre-recorded videos from `/media` as input sources for pipelines (loops and start-time supported)
- **Auto-recovery** — configurable retry and backoff when outputs fail or the input drops
- **System metrics** — CPU, RAM, disk, and network throughput in the navbar
- **HLS preview** — in-dashboard live preview proxied through the app

## Local Development

Install dependencies and build:

```sh
npm ci
npm run build
```

Download the MediaMTX binary for your platform from the [official GitHub releases](https://github.com/bluenviron/mediamtx/releases) and place the executable (`mediamtx` or `mediamtx.exe`) in the repo root. Then start it with the checked-in config:

```sh
./mediamtx        # Linux/macOS
mediamtx.exe      # Windows
```

In another terminal, start the app:

```sh
npm start
```

The dashboard runs on `http://localhost:3030/` by default.

### Development Mode

Run each in its own terminal for live reload:

```sh
npm run dev             # backend with live reload via tsx watch
npm run watch:frontend  # frontend TypeScript in watch mode
npm run watch:css       # Tailwind CSS in watch mode
```

`npm run dev` runs the backend source directly via `tsx` — no compile step needed.

### Build Commands

```sh
npm run build           # backend (src/ → dist/) + frontend (public/ts/ → public/js/) + CSS
npm run build:backend   # backend only
npm run build:frontend  # frontend TS + CSS
```

**Always edit `public/ts/` not `public/js/`** — `public/js/` is generated.

### Other Commands

```sh
npm run format           # run Prettier
npm run format:check     # check formatting (used in CI)
npm run test:routes      # unit tests for REST routes (no external services needed)
npm run test:normalization  # unit tests for URL normalization helpers
npm run test:integration # 2x3 end-to-end test (requires running app + MediaMTX)
```

## Runtime Files

| Path | Description |
|---|---|
| `data.db` | SQLite database — symlink to `/var/lib/restream/data.db` on VM deployments |
| `media/` | Recordings and video ingest sources — symlink to `/var/lib/restream/media/` on VM deployments |
| `public/output.css` | Generated CSS — do not edit |
| `public/js/` | Compiled frontend JS — do not edit |
| `dist/` | Compiled backend JS — do not edit |

## Testing

### Unit Tests

Run without any external services:

```sh
npm run test:routes        # REST route tests
npm run test:normalization # URL normalization helpers
```

### Integration Test (2x3)

Starts RTMP and SRT publishers for two pipelines, starts all six outputs, waits for `on` status, then stops everything. Requires a running app and MediaMTX.

```sh
npm run test:integration
```

Environment overrides:

| Variable | Default | Description |
|---|---|---|
| `API_URL` | `http://localhost:3030` | Backend base URL |
| `MANIFEST_PATH` | `test/artifacts/session-2x3-manifest.json` | Path to manifest |
| `INPUT_PROTOCOLS` | `rtmp,srt` | Comma-separated ingest protocols |
| `TIMEOUT_SEC` | `120` | Seconds to wait for all streams to go green |
| `KEEP_RUNNING` | `0` | Set to `1` to leave resources in place after the run |

### CI

GitHub Actions (`.github/workflows/ci.yml`) runs on every push and pull request:

- **format** — Prettier check
- **unit** — route and normalization tests
- **integration** — 2x3 end-to-end test; installs FFmpeg, downloads MediaMTX, starts both services, runs the test; uploads logs as an artifact on failure

## Linux VM Deployment (GCP)

### Host Requirements

- 2 vCPU, 4 GB RAM, 20 GB disk
- Ubuntu 22.04 or 24.04 with systemd
- Node.js 22, FFmpeg 7.1+, MediaMTX, git — all installed by the setup script

### Initial Setup

SSH into a fresh VM, clone the repo, and run the setup script as root:

```sh
sudo git clone https://github.com/live-miracles/restream /opt/restream
sudo bash /opt/restream/scripts/server-setup.sh
```

The script installs Node.js 22, FFmpeg 7.1 (BtbN static build), MediaMTX 1.17.1, creates a `restream` service user, builds the app, and registers `mediamtx.service` and `restream.service` as systemd units that start on every boot. Both services run as the non-root `restream` user.

### GCP Firewall Rules

Open these ports in your VPC firewall (VPC Network → Firewall):

| Port | Protocol | Purpose |
|---|---|---|
| `3030` | TCP | Direct dashboard access before nginx is configured; close or restrict after HTTPS is live |
| `443` | TCP | HTTPS dashboard when using a reverse proxy |
| `1935` | TCP | RTMP ingest |
| `10080` | UDP/TCP | SRT ingest |

MediaMTX API (`9997`) and HLS preview (`8888`) stay localhost-only.

### Settings

Open `http://<VM-external-IP>:3030/settings.html` to change the server name, manage custom encodings, and configure video ingests.

To edit MediaMTX config and apply it:

```sh
sudo vim /opt/restream/mediamtx.yml
sudo bash /opt/restream/scripts/server-update.sh
```

### Updating

```sh
sudo bash /opt/restream/scripts/server-update.sh
```

Pulls the latest code, rebuilds, copies config to `/etc/restream/`, and restarts both services.

### Operations

Follow logs:

```sh
journalctl -u restream.service -f
journalctl -u mediamtx.service -f
```

Stop services (without disabling boot start):

```sh
sudo bash /opt/restream/scripts/server-down.sh
```

Restart services:

```sh
sudo systemctl restart mediamtx.service
sudo systemctl restart restream.service
```

Check health:

```sh
curl -fsS http://127.0.0.1:3030/healthz
curl -fsS http://127.0.0.1:3030/health
```

Backup data:

```sh
cp /var/lib/restream/data.db /var/lib/restream/data.db.bak-$(date +%F-%H%M%S)
# media files live in /var/lib/restream/media/
```

### Reverse Proxy and TLS

Put nginx in front of port `3030` and terminate TLS at `443`. Keep MediaMTX API (`9997`) bound to localhost. Restrict direct access to port `3030` via firewall once nginx is in place.

The deployment uses a stable self-signed origin certificate for nginx. The public certificate may be shared with the upstream/Cloudflare owner so they can trust the origin. Do not generate a new certificate when the public IP is moved to a different VM; copy the existing certificate and private key to the new VM instead. Regenerating it changes the certificate fingerprint and requires upstream/Cloudflare reconfiguration.

Only share the public certificate (`/etc/ssl/certs/nginx-selfsigned.crt`) outside the VM. The private key (`/etc/ssl/private/nginx-selfsigned.key`) is required only on origin VMs that will terminate HTTPS.

Generate the origin certificate only once on the first origin VM:

```sh
sudo openssl req -x509 -nodes -days 3650 -newkey rsa:2048 \
  -keyout /etc/ssl/private/nginx-selfsigned.key \
  -out /etc/ssl/certs/nginx-selfsigned.crt \
  -subj "/CN=mtx-india-test-v1"
```

When moving the public IP to another VM, copy the same files from the old VM to the same paths on the new VM:

```sh
/etc/ssl/private/nginx-selfsigned.key
/etc/ssl/certs/nginx-selfsigned.crt
```

Then set ownership and permissions on the destination VM:

```sh
sudo chown root:root /etc/ssl/private/nginx-selfsigned.key /etc/ssl/certs/nginx-selfsigned.crt
sudo chmod 600 /etc/ssl/private/nginx-selfsigned.key
sudo chmod 644 /etc/ssl/certs/nginx-selfsigned.crt
```

Do not commit exported certificates or any private key material to the repo.

The nginx site config is checked in at `deploy/nginx/restream.conf`. Install it as the active site config:

```sh
sudo cp /opt/restream/deploy/nginx/restream.conf /etc/nginx/sites-available/restream
sudo ln -sf /etc/nginx/sites-available/restream /etc/nginx/sites-enabled/restream
sudo rm -f /etc/nginx/sites-enabled/default
sudo nginx -t
sudo systemctl restart nginx
```

Make sure the VM has the GCP network tags required by the existing firewall rules:

```sh
gcloud compute instances add-tags <instance-name> \
  --zone=<instance-zone> \
  --tags=http-server,https-server
```

Test from inside the VM:

```sh
curl -fsSI http://127.0.0.1/
curl -k -fsSI https://127.0.0.1/
curl -k -fsS https://127.0.0.1/healthz
```

From outside after the public IP and firewall are in place:

```sh
curl -fsSI http://<VM-external-IP>/
curl -k -fsSI https://<VM-external-IP>/
curl -k -fsS https://<VM-external-IP>/healthz
```

Expected results:

- `http://<VM-external-IP>/` returns `301` and redirects to HTTPS.
- `https://<VM-external-IP>/` returns `200`.
- `https://<VM-external-IP>/healthz` returns `{"status":"ok"}`.

Browsers will show a warning when visiting the VM directly because the certificate is self-signed. Cloudflare/upstream should trust the stable origin certificate separately.

### Security Baseline

- Both services run as non-root (`restream` user).
- Keep `9997` and `8888` local-only.
- Use firewall rules to restrict ingest and UI ports.
- MediaMTX publish/read/playback authorization is delegated to the local Restream auth endpoint,
  which rejects unknown stream keys and temporarily bans IPs after repeated failures.
- For dashboard/API traffic, put HTTPS and request rate limiting at the reverse proxy or Google
  Cloud Armor layer. RTMP/SRT stream-key abuse is handled in the MediaMTX auth callback because
  those protocols are not HTTP requests.
- Rotate stream keys periodically.
- Apply OS and package updates on a maintenance schedule.

## Docs

- [Architecture](docs/architecture.md): system map, data model, and core flows
- [Configuration](docs/configuration.md): environment variables and runtime settings
- [API Reference](docs/api-reference.md): REST endpoints
- [Health Mapping](docs/health-mapping.md): how input and output statuses are derived
