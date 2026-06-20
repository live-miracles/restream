# Legacy MediaMTX Monitoring Notes

This document is retained as migration context for the archived Node.js
implementation under `old/`. MediaMTX is not part of the current production
runtime.

## Previous Architecture

The old backend used MediaMTX for:

- RTMP/SRT publisher and reader transport;
- dynamic path configuration and authorization callbacks;
- path/connection health APIs;
- Prometheus metrics;
- HLS preview;
- protocol session IDs used by diagnostics.

The Node control plane merged MediaMTX state, FFmpeg child progress, ffprobe
results, and SQLite job state into its dashboard health model.

Those statements are historical. They do not describe the Rust router or
`MediaEngine`.

## Rust Replacements

| Previous MediaMTX surface | Current source |
|---|---|
| Publish authorization callback | Native RTMP/SRT stream-key validation |
| Path/connection APIs | `MediaEngine::active_ingests` and `active_egresses` |
| SRT connection quality | Direct `srt_bistats()` samples |
| RTMP connection quality | Linux `TCP_INFO` and `SO_MEMINFO` on the accepted socket |
| HLS preview surface | In-memory `HlsStore` and Axum routes; live media generation still needs packet-contract repair |
| MediaMTX config/status pages | `/api/status`, `/config`, native diagnostics |
| MediaMTX Prometheus endpoint | No Rust equivalent yet |

The current `/health` endpoint is built directly from native state and does not
poll a sidecar.

## Remaining Migration Work

- Remove or replace frontend Grafana links that target old MediaMTX dashboards.
- Replace the old Node/MediaMTX GitHub Actions workflow with Rust-native CI.
- Add a Rust Prometheus exporter if time-series monitoring is still required.
- Keep MediaMTX only as an isolated interoperability sink in protocol tests.

## Historical References

These links remain useful when reading archived code or building an independent
test sink:

- [MediaMTX Control API](https://mediamtx.org/docs/references/control-api)
- [MediaMTX configuration](https://mediamtx.org/docs/references/configuration-file)
- [MediaMTX metrics](https://mediamtx.org/docs/features/metrics)
- [MediaMTX architecture](https://mediamtx.org/docs/features/architecture)
