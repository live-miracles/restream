# Deployment Guide (Host-Only, No Containers)

This guide deploys Restream directly on a Linux host using native services only.

## 1. Target Topology

- `mediamtx` system service
- `restream` (Node.js app) system service
- Optional reverse proxy (nginx/caddy) for TLS termination
- SQLite on local disk (`data/data.db`)

No Docker or container runtime is required.

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
sudo mkdir -p /opt/restream /var/lib/restream /var/log/restream /etc/restream
sudo chown -R restream:restream /opt/restream /var/lib/restream /var/log/restream /etc/restream
```

Recommended layout:

- App: `/opt/restream`
- Data: `/var/lib/restream`
- Logs: journald + `/var/log/restream` (optional)
- MediaMTX config: `/etc/restream/mediamtx.yml`

## 4. Install Application

```sh
sudo -u restream git clone https://github.com/live-miracles/restream /opt/restream
cd /opt/restream
sudo -u restream npm ci --omit=dev
```

Set config file:

```sh
sudo -u restream cp src/config/restream.json /etc/restream/restream.json
```

Adjust values in `/etc/restream/restream.json` for server name and limits.

## 5. Install and Configure MediaMTX

1. Install MediaMTX binary in `/usr/local/bin/` from official [releases](https://github.com/bluenviron/mediamtx/releases).
2. Place config:

```sh
sudo cp /opt/restream/infra/mediamtx.yml /etc/restream/mediamtx.yml
sudo chown restream:restream /etc/restream/mediamtx.yml
```

3. Ensure API binding remains local-only:

- `apiAddress: 127.0.0.1:9997`

4. Open ingest ports to publishers as needed:

- 1935 (RTMP)
- 8554 (RTSP)
- 8890 (SRT)

## 6. Systemd Unit: MediaMTX

Create `/etc/systemd/system/mediamtx.service`:

```ini
[Unit]
Description=MediaMTX Streaming Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=restream
Group=restream
ExecStart=/usr/local/bin/mediamtx /etc/restream/mediamtx.yml
Restart=always
RestartSec=2
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
```

## 7. Systemd Unit: Restream

Create `/etc/systemd/system/restream.service`:

```ini
[Unit]
Description=Restream Control Plane
After=network-online.target mediamtx.service
Wants=network-online.target
Requires=mediamtx.service

[Service]
Type=simple
User=restream
Group=restream
WorkingDirectory=/opt/restream
Environment=NODE_ENV=production
Environment=PORT=3030
Environment=RESTREAM_CONFIG_PATH=/etc/restream/restream.json
Environment=FFMPEG_PATH=/usr/bin/ffmpeg
Environment=FFPROBE_PATH=/usr/bin/ffprobe
ExecStart=/usr/bin/node /opt/restream/src/index.js
Restart=always
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ReadWritePaths=/var/lib/restream /opt/restream/data

[Install]
WantedBy=multi-user.target
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

1. Pull latest code in `/opt/restream`.
2. Run `npm ci --omit=dev`.
3. Restart `restream.service`.
4. Run smoke tests.

## 12. Security Baseline

- Run services as non-root user.
- Keep `9997` local-only.
- Use firewall rules for ingest and UI ports.
- Rotate stream keys periodically.
- Apply OS and package updates on a maintenance schedule.
