# Logging Architecture

Restream uses the `tracing` ecosystem as a unified logging facade. All
`println!`/`eprintln!` callsites are replaced with structured macros
(`error!`, `warn!`, `info!`, `debug!`, `trace!`). A single subscriber
fans events out to four independent sinks simultaneously. Process logs
are queryable and streamable via `/api/logs` and `/api/logs/stream`.

This document covers the logging architecture only. The former `job_logs`
and `lifecycle_events` tables have been removed; all durable log storage
now goes through `app_logs`. The former `/api/pipelines/:id/history` and
`.../outputs/:oid/history` endpoints have been removed; the history UI
calls `/api/logs` with `pipeline_id`/`output_id`/`event_class` filters.

## Overview

```text
callsites (error! warn! info! debug! trace!)
    |
    v
tracing facade  (compile-time level strip via Cargo features)
    |
    v
tracing_subscriber::Registry + EnvFilter
    |
    +---> fmt::Layer      --> stdout / stderr
    +---> FileLayer       --> logs/restream.YYYY-MM-DD  (NonBlocking)
    +---> DbLayer         --> app_logs table (SQLite, batched)
    `---> BroadcastLayer  --> tokio broadcast channel --> SSE subscribers
                                                          (GET /api/logs/stream)
```

Hot paths (`src/media/ring_buffer.rs`, `src/media/avio.rs`) carry zero
logging. The architecture enforces this by convention: those modules
contain no tracing macro calls, and the CI clippy pass will catch any
accidental addition.

## Level Policy

Levels follow RFC 5424 severity ordering, narrowed to five:

| Level | Used for |
|---|---|
| `error` | Unrecoverable faults: socket failures that stop a task, FFmpeg crashes, DB write failures |
| `warn` | Recoverable problems: retries, dropped SSE subscriber, bounded channel full |
| `info` | Lifecycle transitions: server start, ingest connect/disconnect, egress start/stop, shutdown |
| `debug` | Per-connection diagnostics: reconnect attempts, bond membership changes, ring buffer fills |
| `trace` | Reserved — not used in production. Available for one-off local investigation only. |

### Compile-time stripping

```toml
# Cargo.toml  — dev / CI
[dependencies]
tracing = { version = "0.1", features = ["max_level_debug"] }

# Cargo.toml  — release / bench profile
# strip debug + trace at compile time; call sites become dead code
[profile.bench.package.tracing]
# set via feature flag in Cargo.toml profile section:
# tracing = { version = "0.1", features = ["release_max_level_info"] }
```

At compile time with `release_max_level_info`, the `debug!` and `trace!`
macros expand to nothing. The optimizer removes the branches entirely —
no string formatting, no function call overhead. `error`, `warn`, and
`info` remain fully instrumented.

For extreme performance investigation where even `info` overhead is
measurable, rebuild with `release_max_level_warn`.

### Runtime filtering

At runtime, `RUST_LOG` controls which compiled-in levels actually reach
subscribers:

```sh
# default
RUST_LOG=info

# verbose SRT debugging without flooding other modules
RUST_LOG=info,restream::media::srt=debug

# silence noisy module
RUST_LOG=info,restream::media::h264_transcoder=warn
```

`EnvFilter` is evaluated once per event before any sink is touched.

## Sinks

### 1. fmt::Layer — stdout / stderr

`error!` events go to stderr; all others go to stdout. Format is compact
text in development and JSON in production (controlled by
`RESTREAM_LOG_FORMAT=json|compact`, default `compact`).

JSON format makes log lines parseable by Datadog, Loki, Cloud Logging,
and similar without a parsing rule.

### 2. FileLayer — rolling file

Uses `tracing-appender::rolling::daily("logs/", "restream.log")` wrapped
in `non_blocking()`. The file sink runs in a background OS thread;
callsites never block on disk I/O.

The `WorkerGuard` returned by `non_blocking()` is held for the process
lifetime in `run_app()`. On shutdown the guard is dropped last, flushing
any buffered lines before the process exits.

File names: `logs/restream.2026-06-27`. No log rotation library is
required — daily rotation is built into `tracing-appender`.

Configure the directory with `RESTREAM_LOG_DIR` (default: `logs/`). Set
to an empty string to disable file logging.

### 3. DbLayer — SQLite (`app_logs` table)

The `DbLayer::on_event()` implementation does one thing: push a
`LogEntry` onto a bounded `mpsc` channel (capacity 4096). A dedicated
tokio task drains the channel and batch-inserts rows every 100 ms using
a single `INSERT INTO app_logs VALUES (...),(...),...`.

If the channel is full (consumer can't keep up), the layer calls
`try_send` and silently drops the entry rather than blocking the
callsite. A counter tracks drops and is included in the next successful
batch via a synthetic `warn` row.

Only `≥ info` level events are written to the database. `debug` and
`trace` are filtered before the channel send.

#### Schema

```sql
CREATE TABLE IF NOT EXISTS app_logs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT    NOT NULL,           -- RFC3339
    level       TEXT    NOT NULL,           -- ERROR / WARN / INFO / DEBUG
    target      TEXT    NOT NULL,           -- module path: "restream::media::srt"
    message     TEXT    NOT NULL,
    fields      TEXT,                       -- JSON object of structured fields
    pipeline_id TEXT,                       -- populated from enclosing span when available
    output_id   TEXT                        -- populated from enclosing span when available
);

CREATE INDEX idx_app_logs_ts       ON app_logs(ts DESC);
CREATE INDEX idx_app_logs_level    ON app_logs(level, ts DESC);
CREATE INDEX idx_app_logs_target   ON app_logs(target, ts DESC);
CREATE INDEX idx_app_logs_pipeline ON app_logs(pipeline_id, ts DESC)
    WHERE pipeline_id IS NOT NULL;
```

Retention is controlled by `RESTREAM_LOG_RETENTION_DAYS` (default: 7).
A cleanup task runs once per hour and calls
`DELETE FROM app_logs WHERE ts < datetime('now', '-N days')`.

### 4. BroadcastLayer — SSE live tail

`BroadcastLayer::on_event()` calls `broadcast::Sender::send()` on a
channel with capacity 256. If all receivers are caught up this is a
non-blocking clone of the `LogEntry`. If a receiver's buffer is full,
`broadcast` automatically marks it as lagged and the next `recv()` on
that receiver returns `RecvError::Lagged` — the SSE handler closes that
connection so the client reconnects and backfills from the database.

Only `≥ info` level events are broadcast.

## Spans and Pipeline Context

`tracing` spans propagate structured context to all child events. Wrap
long-lived work units in a span so that `pipeline_id` and `output_id`
are injected automatically:

```rust
// In the reconciler before spawning an egress task:
let span = tracing::info_span!(
    "egress",
    pipeline_id = %pipeline_id,
    output_id   = %output_id,
);
let _guard = span.enter();

// All info!/warn!/error! inside this scope carry pipeline_id + output_id.
// DbLayer extracts these from the span's field set and populates the
// indexed columns in app_logs.
info!(url = %target_url, "starting egress");
```

This lets `/api/logs?pipeline_id=abc123` return all log entries for a
pipeline across every module without manual tagging at each callsite.

## API

### `GET /api/logs`

Returns paginated log entries from the `app_logs` table.

**Authentication:** session token required (same as all authenticated endpoints).

**Query parameters:**

| Parameter | Type | Default | Description |
|---|---|---|---|
| `level` | `error\|warn\|info\|debug` | `info` | Minimum level |
| `since` | RFC3339 | — | Inclusive lower bound on `ts` |
| `until` | RFC3339 | — | Exclusive upper bound on `ts` |
| `target` | string | — | Module prefix filter (`restream::media::srt`) |
| `pipeline_id` | string | — | Restrict to a single pipeline |
| `output_id` | string | — | Restrict to a single output (requires `pipeline_id`) |
| `limit` | integer | `200` | 1–1000 |
| `order` | `asc\|desc` | `desc` | Sort order on `ts` |

**Response:**

```json
{
  "logs": [
    {
      "id": 4522,
      "ts": "2026-06-27T14:23:05.123Z",
      "level": "INFO",
      "target": "restream::media::srt",
      "message": "ingest connected",
      "fields": { "stream_id": "live/abc", "remote": "1.2.3.4:5000" },
      "pipelineId": "abc123",
      "outputId": null
    }
  ],
  "total": 1,
  "hasMore": false
}
```

### `GET /api/logs/stream`

SSE live tail of new log entries. Accepts the same query parameters as
`GET /api/logs` for filtering.

**Connection flow:**

1. On connect, handler reads `Last-Event-ID` header (or `?last_event_id=`
   query param) and backfills any entries with `id > last_event_id` from
   the database as `event: log` frames.
2. Handler subscribes to the broadcast channel.
3. Incoming broadcast entries that match the caller's filter are sent as
   `event: log` frames.
4. A `event: ping` comment (`: ping`) is sent every 20 seconds to keep
   the connection alive through proxies.

**SSE frame format:**

```
id: 4523
event: log
data: {"id":4523,"ts":"2026-06-27T14:23:06Z","level":"WARN","target":"restream::media::engine","message":"ring buffer 80% full","fields":{"fill":0.80},"pipelineId":"abc123","outputId":null}

: ping

```

**Client reconnection:** browsers reconnect automatically on dropped SSE
connections and send `Last-Event-ID: 4523`. The handler backfills missed
entries from the database, then resumes the live tail. No entries are
lost as long as the reconnect happens within the `app_logs` retention
window (default 7 days).

**Response headers:**

```
Content-Type:      text/event-stream
Cache-Control:     no-cache
X-Accel-Buffering: no
```

`X-Accel-Buffering: no` disables nginx upstream buffering if nginx sits
in front. Cloudflare Tunnel (cloudflared) and GCP HTTP Load Balancer
both pass chunked transfer responses through without buffering when
`Content-Type: text/event-stream` is set.

**Infrastructure notes:**

- **Cloudflare Tunnel:** cloudflared runs on the VM and streams bytes
  directly to CF's edge. No response buffering in the tunnel path.
  No special CF configuration required.
- **GCP HTTP LB:** set the backend service timeout to `3600` seconds.
  The default 30s timeout tears down streaming connections.
  Chunked transfer is supported natively.

## `src/logging.rs`

All subscriber construction lives in a single module: `src/logging.rs`.
`run_app()` calls `logging::init()` as its first action, before the
tokio runtime spawns any tasks.

```rust
pub struct LoggingHandles {
    pub log_tx:      mpsc::Sender<LogEntry>,      // DbLayer feed
    pub broadcast_tx: broadcast::Sender<LogEntry>, // SSE feed
    _file_guard:     tracing_appender::non_blocking::WorkerGuard,
}

pub fn init(db_pool: SqlitePool) -> LoggingHandles { ... }
```

`LoggingHandles` is stored in `AppState` so that:
- `broadcast_tx` can be cloned into each `/api/logs/stream` handler.
- `_file_guard` is kept alive for the process lifetime.
- The DB drain task holds `log_tx`'s paired receiver.

## Dependency additions

```toml
[dependencies]
tracing            = { version = "0.1", features = ["max_level_debug"] }
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
tracing-appender   = "0.2"
```

No other new dependencies. `tokio::sync::{mpsc, broadcast}`, `sqlx`, and
`axum` SSE patterns are already in the tree.

## Callsite conventions

All `println!`/`eprintln!` calls have been replaced with tracing macros.
The `[tag]` prefix that appeared in print messages is gone — the `target`
field (module path) carries that information automatically.

```rust
// ✗ old
eprintln!("[srt] socket error: {}", e);

// ✓ new
error!(err = %e, "socket error");
```

Lifecycle transition events carry `event_class = "lifecycle"` and
`event_type` fields so the history UI can filter them:

```rust
info!(
    pipeline_id = %pipeline_id,
    output_id   = %output_id,
    event_class = "lifecycle",
    event_type  = "lifecycle.start",
    "output job started",
);
```

Never log inside packet-level loops in `src/media/ring_buffer.rs` or
`src/media/avio.rs` (push, pull, read). Control operations such as
creation, resize, or reader registration are not hot and may use `debug!`
or `info!`.

## Invariants

- No `println!` or `eprintln!` in `src/` after migration (enforced by a
  `clippy::print_stdout` / `clippy::print_stderr` deny in `src/lib.rs`).
- No logging macros inside packet-level loops in `ring_buffer.rs` or `avio.rs` (push/pull/read). Control-plane paths (creation, resize, reader registration) may log.
- `DbLayer::on_event()` and `BroadcastLayer::on_event()` must not block.
  Both use `try_send` / non-blocking broadcast send only.
- `WorkerGuard` for the file sink must outlive all log calls — hold it in
  `LoggingHandles` inside `AppState`, drop after `shutdown()` completes.
- Spans providing `pipeline_id` context must be entered before any
  `tokio::spawn` that should carry the context; use `.instrument(span)`
  on the future, not `span.enter()`, for async tasks.
