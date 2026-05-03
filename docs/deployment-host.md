# Deployment Guide (Host-Only, No Containers)

This guide deploys Restream directly on a Linux host using native services only.

## 1. Target Topology

- `mediamtx` system service
- `restream` (Node.js app) system service
- Optional reverse proxy (nginx/caddy) for TLS termination
- SQLite on local disk (`data/data.db`)

No Docker or container runtime is required.

This guide intentionally avoids `make deps` and `make run-host`. Those helpers target local workstation flows and still perform broader preflight checks for optional Docker-backed targets such as `nginx-rtmp`, `make run-docker`, and `make run-4x3`.

To reduce copy/paste, this guide uses two targets:

- `make build` stages a deployment bundle under `build/deploy/`
- `make install-system` copies that bundle into the live `/opt/restream`, `/etc/restream`, and `/etc/systemd/system` paths using `sudo`

The bundle contains the runtime app files (`src/`, `public/`, package files, and production-only `node_modules/`) plus the default config and systemd unit files. The source-of-truth files live in `src/config/restream.json`, `infra/mediamtx.yml`, `infra/restream.service`, and `infra/mediamtx.service`.

## 2. Host Requirements

Minimum recommended:

- 2 vCPU
- 4 GB RAM
- 20 GB disk
- Linux with systemd

Software:

- Node.js 22 LTS (or any supported `>=20.19.0 <26`)
- npm `>=10`
- ffmpeg/ffprobe
- MediaMTX
- git (for deployment from source)

## 3. Create Service User and Paths

```sh
sudo useradd --system --home /opt/restream --shell /usr/sbin/nologin restream
sudo mkdir -p /opt/restream-src /opt/restream /var/lib/restream /var/log/restream /etc/restream
sudo chown -R restream:restream /opt/restream-src /opt/restream /var/lib/restream /var/log/restream /etc/restream
```

Recommended layout:

- Source checkout: `/opt/restream-src`
- App: `/opt/restream`
- Data: `/var/lib/restream`
- Logs: journald + `/var/log/restream` (optional)
- MediaMTX config: `/etc/restream/mediamtx.yml`

## 4. Install Application

```sh
sudo -u restream git clone https://github.com/live-miracles/restream /opt/restream-src
cd /opt/restream-src
sudo -u restream make build
make install-system
```

This stages the default deployment files at `/opt/restream-src/build/deploy/`:

- `opt/restream/` — runtime app bundle with `src/`, `public/`, package files, and production `node_modules/`
- `etc/restream/restream.json`
- `etc/restream/mediamtx.yml`
- `etc/systemd/system/restream.service`
- `etc/systemd/system/mediamtx.service`

`make build` runs `npm ci --omit=dev` inside the staged app bundle, so deployment artifacts do not include development dependencies.

`make install-system` copies that staged bundle into:

- `/opt/restream`
- `/etc/restream`
- `/etc/systemd/system`

The staged config and unit files come directly from these repo files:

- `infra/restream.service`
- `infra/mediamtx.service`

Adjust values in `/etc/restream/restream.json` for server name and limits after `make install-system` if needed.

If you want to review or customize the staged files before installing them, inspect `build/deploy/` first and then run `make install-system`.

Set config file manually instead of `make install-system`:

```sh
sudo install -m 0644 build/deploy/etc/restream/restream.json /etc/restream/restream.json
sudo chown restream:restream /etc/restream/restream.json
```

## 5. Install and Configure MediaMTX

1. Install the MediaMTX binary from official [releases](https://github.com/bluenviron/mediamtx/releases).

   For a systemd-managed host, placing it at `/usr/local/bin/mediamtx` is the simplest option.
  If you want to mirror the repo-managed layout used by `scripts/up.sh`, place it at `/opt/restream/bin/mediamtx/mediamtx` and edit the staged `mediamtx.service` file before installing it.
2. Place config manually instead of `make install-system`:

```sh
sudo install -m 0644 /opt/restream-src/build/deploy/etc/restream/mediamtx.yml /etc/restream/mediamtx.yml
sudo chown restream:restream /etc/restream/mediamtx.yml
```

3. Ensure API and HLS bindings remain local-only:

- `apiAddress: 127.0.0.1:9997`
- `hlsAddress: 127.0.0.1:8888`

4. Preview latency versus resource use:

- The checked-in `mediamtx.yml` keeps HLS muxers on-demand with `hlsAlwaysRemux: no` and uses
  `hlsVariant: mpegts`.
- This reduces steady CPU/RAM use when many inputs are idle, because MediaMTX does not maintain
  active HLS muxers for every ready path.
- The tradeoff is slower first-preview startup delay in the dashboard, since MediaMTX must spin up
  a fresh HLS muxer on the first viewer request.

5. Open ingest ports to publishers as needed:

- 1935 (RTMP)
- 8554 (RTSP)
- 8890 (SRT)

## 6. Systemd Unit: MediaMTX

`make install-system` installs the generated unit into `/etc/systemd/system/mediamtx.service`.

Install the generated unit manually instead:

```sh
sudo install -m 0644 /opt/restream-src/build/deploy/etc/systemd/system/mediamtx.service /etc/systemd/system/mediamtx.service
```

If you chose the repo-managed binary path instead, update `build/deploy/etc/systemd/system/mediamtx.service` before installing the unit.

## 7. Systemd Unit: Restream

`make install-system` installs the generated unit into `/etc/systemd/system/restream.service`.

Install the generated unit manually instead:

```sh
sudo install -m 0644 /opt/restream-src/build/deploy/etc/systemd/system/restream.service /etc/systemd/system/restream.service
```

Link runtime DB path to persistent storage (optional but recommended):

```sh
sudo -u restream ln -sfn /var/lib/restream /opt/restream/data
```

## 8. Start Services

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now mediamtx.service
sudo systemctl enable --now restream.service
```

Check status:

```sh
sudo systemctl status mediamtx.service
sudo systemctl status restream.service
```

## 9. Smoke Tests

Backend readiness:

```sh
curl -fsS http://127.0.0.1:3030/healthz
```

Health snapshot:

```sh
curl -fsS http://127.0.0.1:3030/health
```

UI:

- Open `http://<host>:3030/`

## 10. Reverse Proxy and TLS (Recommended)

Put a reverse proxy in front of port `3030` and terminate TLS at `443`.

At minimum:

- Allow only proxy-to-app local traffic for `3030`.
- Keep MediaMTX API (`9997`) bound to localhost.
- Expose ingest ports intentionally and document who can publish.

## 11. Operations Runbook

Logs:

```sh
journalctl -u mediamtx.service -f
journalctl -u restream.service -f
```

Restart:

```sh
sudo systemctl restart mediamtx.service
sudo systemctl restart restream.service
```

Backup SQLite:

```sh
cp /var/lib/restream/data.db /var/lib/restream/data.db.bak-$(date +%F-%H%M%S)
```

Upgrade process:

1. Pull latest code in `/opt/restream-src`.
2. Run `make build`.
3. Run `make install-system`.
4. Upgrade the MediaMTX binary if the release version changed.
5. Restart `mediamtx.service` and `restream.service`.
6. Run smoke tests.
7. Validate dashboard and stream-keys pages after a normal refresh.

### Frontend Cache Note

Static UI assets use split cache policy: HTML documents are served with `Cache-Control: no-store`, while JS/CSS are served with `Cache-Control: public, max-age=0, must-revalidate` plus ETag/Last-Modified. During rollout verification:

- use a normal refresh first (the browser should revalidate and fetch current JS/CSS)
- verify the latest JS is loaded before debugging runtime behavior
- if an upstream proxy/CDN overrides cache policy, force refresh once and fix proxy cache rules

## 12. Security Baseline

- Run services as non-root user.
- Keep `9997` local-only.
- Use firewall rules for ingest and UI ports.
- Rotate stream keys periodically.
- Apply OS and package updates on a maintenance schedule.
