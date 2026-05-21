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
- a starter `SRT Connection Health` dashboard

Restream proxies Grafana at `/grafana/`:

```text
http://127.0.0.1:3030/grafana/
```

Keep Grafana's own `3000` listener localhost-only. Browser access should go through the same public
entry point as the Restream dashboard, while Grafana continues to query the local Prometheus
datasource. The proxy target defaults to `http://127.0.0.1:3000` and can be changed with
`GRAFANA_PROXY_TARGET`.

For an extra gate in simple deployments, set `GRAFANA_PROXY_TOKEN`. The proxy then accepts either an
`Authorization: Bearer <token>` header or a one-time `?grafana_token=<token>` visit that sets an
HTTP-only cookie scoped to the Grafana proxy path. In production, prefer putting the whole Restream
origin behind normal HTTPS and authentication as well.

The dashboard includes Grafana buttons for each pipeline and output. They open the MediaMTX Overview
dashboard in a new tab with `var-path=live/<streamKey>`. Output buttons also pass `var-output` as
operator context; MediaMTX metrics remain path-based until Restream exposes output-level Prometheus
metrics.

## Docker Option

For the Linux VM deployment shape, Prometheus and Grafana can be run with host networking:

```sh
cd monitoring
docker compose up -d
```

Ports:

| Port | Purpose |
|---|---|
| `9090` | Prometheus localhost listener |
| `3000` | Grafana localhost listener |

The starter Grafana login is `admin` / `admin`. Change it before any shared or production use.

This Docker Compose file is aimed at Linux hosts because it uses `network_mode: host`, allowing
Prometheus to scrape `127.0.0.1:9998` without exposing MediaMTX metrics publicly. It binds
Prometheus and Grafana to `127.0.0.1`, so no public firewall rule is needed for `9090` or `3000`.
On macOS Docker Desktop, prefer running Prometheus directly on the host, or change the scrape target
to `host.docker.internal:9998` for local experiments.

## What To Graph First

Useful starter queries:

```promql
up{job="mediamtx"}
sum(paths{job="mediamtx",name=~"$path"})
sum by (state) (paths{job="mediamtx",name=~"$path"})
sum(paths_readers{job="mediamtx",name=~"$path"})
(sum(rate(paths_inbound_bytes{job="mediamtx",name=~"$path"}[1m])) or sum(rate(paths_bytes_received{job="mediamtx",name=~"$path"}[1m]))) * 8
(sum(rate(paths_outbound_bytes{job="mediamtx",name=~"$path"}[1m])) or sum(rate(paths_bytes_sent{job="mediamtx",name=~"$path"}[1m]))) * 8
sum(rate(paths_inbound_frames_in_error{job="mediamtx",name=~"$path"}[5m])) or vector(0)
```

These cover scrape health, active paths, reader count, path states, throughput, and ingest frame
errors. The byte queries include both newer and older MediaMTX metric names so local development
and the pinned Linux VM version can use the same starter dashboard. Protocol-specific panels can be
added once the live traffic shape is clear.

## SRT Connection Health Dashboard

The `SRT Connection Health` dashboard is based on the SRT metrics listed in the
[MediaMTX metrics documentation](https://mediamtx.org/docs/features/metrics). It focuses on:

- active SRT connection count
- RTT
- send and receive rate
- link capacity
- send and receive loss rate
- retransmit, drop, and undecrypt counters
- send and receive buffer pressure
- flight size, flow window, and NAK counters

Useful SRT queries:

```promql
sum(srt_conns{job="mediamtx",path=~"$path"})
avg(srt_conns_ms_rtt{job="mediamtx",path=~"$path"})
sum(srt_conns_mbps_send_rate{job="mediamtx",path=~"$path"})
sum(srt_conns_mbps_receive_rate{job="mediamtx",path=~"$path"})
sum(srt_conns_mbps_link_capacity{job="mediamtx",path=~"$path"})
avg(srt_conns_packets_send_loss_rate{job="mediamtx",path=~"$path"})
avg(srt_conns_packets_received_loss_rate{job="mediamtx",path=~"$path"})
sum(srt_conns_packets_retrans{job="mediamtx",path=~"$path"})
sum(srt_conns_packets_received_retrans{job="mediamtx",path=~"$path"})
sum(srt_conns_packets_send_drop{job="mediamtx",path=~"$path"})
sum(srt_conns_packets_received_drop{job="mediamtx",path=~"$path"})
sum(srt_conns_packets_received_undecrypt{job="mediamtx",path=~"$path"})
```
