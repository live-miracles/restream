# Observability

Restream uses two observability layers:

| Layer | Endpoint | Purpose |
|---|---|---|
| Restream health | `http://127.0.0.1:3030/health` | Current control-plane status for the dashboard |
| Restream host metrics | `http://127.0.0.1:3030/metrics/system` | JSON CPU, RAM, disk, and network values for the dashboard navbar |
| MediaMTX metrics | `http://127.0.0.1:9998/metrics` | Prometheus-compatible time-series metrics for Prometheus and Grafana |

`/health` and `/metrics/system` are JSON APIs. They are intentionally not Prometheus scrape
targets today. MediaMTX already exposes Prometheus-compatible metrics, so Prometheus should scrape
MediaMTX directly.

## MediaMTX Metrics

MediaMTX metrics are enabled in `mediamtx.yml`:

```yaml
metrics: yes
metricsAddress: 127.0.0.1:9998
```

Keep the metrics listener local-only. Prometheus should run on the same host, or in a trusted
host-network container on that host.

Quick check after MediaMTX is running:

```sh
curl -fsS http://127.0.0.1:9998/metrics | head
```

## Prometheus

A starter Prometheus config is checked in at `monitoring/prometheus.yml`.

For a host-installed Prometheus:

```sh
prometheus --config.file=monitoring/prometheus.yml
```

Then open:

```text
http://127.0.0.1:9090/targets
```

The `mediamtx` target should be `UP`.

## Grafana

The `monitoring/grafana/` directory contains provisioning files for:

- a Prometheus datasource at `http://127.0.0.1:9090`
- a starter `MediaMTX Overview` dashboard

## Docker Option

For the Linux VM deployment shape, Prometheus and Grafana can be run with host networking:

```sh
cd monitoring
docker compose up -d
```

Ports:

| Port | Purpose |
|---|---|
| `9090` | Prometheus |
| `3000` | Grafana |

The starter Grafana login is `admin` / `admin`. Change it before any shared or production use.

This Docker Compose file is aimed at Linux hosts because it uses `network_mode: host`, allowing
Prometheus to scrape `127.0.0.1:9998` without exposing MediaMTX metrics publicly. On macOS
Docker Desktop, prefer running Prometheus directly on the host, or change the scrape target to
`host.docker.internal:9998` for local experiments.

## What To Graph First

Useful starter queries:

```promql
up{job="mediamtx"}
sum(paths{job="mediamtx"})
sum by (state) (paths{job="mediamtx"})
sum(paths_readers{job="mediamtx"})
(sum(rate(paths_inbound_bytes{job="mediamtx"}[1m])) or sum(rate(paths_bytes_received{job="mediamtx"}[1m]))) * 8
(sum(rate(paths_outbound_bytes{job="mediamtx"}[1m])) or sum(rate(paths_bytes_sent{job="mediamtx"}[1m]))) * 8
sum(rate(paths_inbound_frames_in_error{job="mediamtx"}[5m])) or vector(0)
```

These cover scrape health, active paths, reader count, path states, throughput, and ingest frame
errors. The byte queries include both newer and older MediaMTX metric names so local development
and the pinned Linux VM version can use the same starter dashboard. Protocol-specific panels can be
added once the live traffic shape is clear.
