//! Unified logging subsystem — tracing subscriber with four simultaneous sinks:
//!
//! 1. **fmt::Layer**       → stdout (info/debug) + stderr (error/warn)
//! 2. **FileLayer**        → rolling daily file via `tracing-appender` (NonBlocking)
//! 3. **DbLayer**          → `app_logs` SQLite table, batched every 100 ms
//! 4. **BroadcastLayer**   → `tokio::sync::broadcast` channel for SSE live tail
//!
//! Call `init()` once at the top of `run_app()`, before any tasks are spawned.
//! Hold the returned `LoggingHandles` for the process lifetime.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Id, Metadata, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Layer};

use crate::types::AppLogEntry;

static CORRELATION_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn next_correlation_id(prefix: &str) -> String {
    let seq = CORRELATION_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{seq:016x}")
}

// ── Public broadcast entry (also used by the SSE handler) ────────────────────

/// A single log entry broadcast to SSE subscribers and written to SQLite.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogBroadcast {
    pub id: i64, // set to 0 until confirmed by DB; SSE uses this for Last-Event-ID
    pub ts: String,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Option<String>,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    pub event_type: Option<String>,
}

// ── LoggingHandles ────────────────────────────────────────────────────────────

/// Held by `AppState` for the process lifetime.
/// Dropping this struct flushes the file sink and stops the DB drain task.
pub struct LoggingHandles {
    pub broadcast_tx: broadcast::Sender<LogBroadcast>,
    _db_tx: tokio::sync::mpsc::Sender<AppLogEntry>,
    _file_guard: WorkerGuard,
}

// ── Span context store ────────────────────────────────────────────────────────

/// Tracks span field values we care about (pipeline_id, output_id) so that
/// events emitted inside a span can inherit those values into app_logs.
#[derive(Default, Clone)]
struct SpanFields {
    pipeline_id: Option<String>,
    output_id: Option<String>,
}

#[derive(Default, Clone)]
struct SpanStore(Arc<Mutex<HashMap<Id, SpanFields>>>);

impl SpanStore {
    fn record(&self, id: &Id, fields: SpanFields) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(id.clone(), fields);
        }
    }

    fn remove(&self, id: &Id) {
        if let Ok(mut m) = self.0.lock() {
            m.remove(id);
        }
    }

    fn lookup<S>(&self, ctx: &Context<'_, S>) -> SpanFields
    where
        S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        let Ok(m) = self.0.lock() else {
            return SpanFields::default();
        };
        let mut result = SpanFields::default();
        if let Some(scope) = ctx.lookup_current() {
            for span in scope.scope() {
                if let Some(sf) = m.get(&span.id()) {
                    if result.pipeline_id.is_none() {
                        result.pipeline_id.clone_from(&sf.pipeline_id);
                    }
                    if result.output_id.is_none() {
                        result.output_id.clone_from(&sf.output_id);
                    }
                }
                if result.pipeline_id.is_some() && result.output_id.is_some() {
                    break;
                }
            }
        }
        result
    }
}

// ── Field visitor ─────────────────────────────────────────────────────────────

struct FieldCollector {
    message: Option<String>,
    pipeline_id: Option<String>,
    output_id: Option<String>,
    event_type: Option<String>,
    event_class: Option<String>,
    extras: Vec<(String, String)>,
}

impl FieldCollector {
    fn new() -> Self {
        Self {
            message: None,
            pipeline_id: None,
            output_id: None,
            event_type: None,
            event_class: None,
            extras: Vec::new(),
        }
    }

    fn fields_json(&self) -> Option<String> {
        if self.extras.is_empty() {
            return None;
        }
        let mut map = serde_json::Map::new();
        for (k, v) in &self.extras {
            map.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        serde_json::to_string(&map).ok()
    }
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{:?}", value).trim_matches('"').to_string();
        match field.name() {
            "message" => self.message = Some(s),
            "pipeline_id" => self.pipeline_id = Some(s),
            "output_id" => self.output_id = Some(s),
            "event_type" => self.event_type = Some(s),
            "event_class" => self.event_class = Some(s),
            _ => self.extras.push((field.name().to_string(), s)),
        }
    }
}

// ── Span attribute visitor ────────────────────────────────────────────────────

struct SpanVisitor(SpanFields);

impl Visit for SpanVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "pipeline_id" => self.0.pipeline_id = Some(value.to_string()),
            "output_id" => self.0.output_id = Some(value.to_string()),
            _ => {}
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{:?}", value).trim_matches('"').to_string();
        match field.name() {
            "pipeline_id" => self.0.pipeline_id = Some(s),
            "output_id" => self.0.output_id = Some(s),
            _ => {}
        }
    }
}

// ── DbLayer ───────────────────────────────────────────────────────────────────

struct DbLayer {
    tx: tokio::sync::mpsc::Sender<AppLogEntry>,
    spans: SpanStore,
}

impl<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>> Layer<S> for DbLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut v = SpanVisitor(SpanFields::default());
        attrs.record(&mut v);
        if v.0.pipeline_id.is_some() || v.0.output_id.is_some() {
            self.spans.record(id, v.0);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        if let Ok(mut m) = self.spans.0.lock() {
            if let Some(sf) = m.get_mut(id) {
                let mut v = SpanVisitor(sf.clone());
                values.record(&mut v);
                *sf = v.0;
            }
        }
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        self.spans.remove(&id);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        // Only persist INFO and above to the database.
        if *meta.level() > tracing::Level::INFO {
            return;
        }

        let mut fc = FieldCollector::new();
        event.record(&mut fc);

        let span_ctx = self.spans.lookup(&ctx);
        // Extract fields_json before any moves of fc's fields.
        let fields = fc.fields_json();
        let message = fc.message.take().unwrap_or_default();
        let pipeline_id = fc.pipeline_id.take().or(span_ctx.pipeline_id);
        let output_id = fc.output_id.take().or(span_ctx.output_id);
        let event_type = fc.event_type.take();
        let event_class = fc.event_class.take();
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let level = meta.level().to_string().to_uppercase();

        let entry = AppLogEntry {
            ts,
            level,
            target: meta.target().to_string(),
            message,
            fields,
            pipeline_id,
            output_id,
            event_type,
            event_class,
        };

        // try_send — never block the callsite; silently drop if channel is full.
        let _ = self.tx.try_send(entry);
    }
}

// ── BroadcastLayer ────────────────────────────────────────────────────────────

struct BroadcastLayer {
    tx: broadcast::Sender<LogBroadcast>,
    spans: SpanStore,
}

impl<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>> Layer<S>
    for BroadcastLayer
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut v = SpanVisitor(SpanFields::default());
        attrs.record(&mut v);
        if v.0.pipeline_id.is_some() || v.0.output_id.is_some() {
            self.spans.record(id, v.0);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        if let Ok(mut m) = self.spans.0.lock() {
            if let Some(sf) = m.get_mut(id) {
                let mut v = SpanVisitor(sf.clone());
                values.record(&mut v);
                *sf = v.0;
            }
        }
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        self.spans.remove(&id);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > tracing::Level::INFO {
            return;
        }

        let mut fc = FieldCollector::new();
        event.record(&mut fc);

        let span_ctx = self.spans.lookup(&ctx);
        let fields = fc.fields_json();
        let message = fc.message.take().unwrap_or_default();
        let pipeline_id = fc.pipeline_id.take().or(span_ctx.pipeline_id);
        let output_id = fc.output_id.take().or(span_ctx.output_id);
        let event_type = fc.event_type.take();
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let level = meta.level().to_string().to_uppercase();

        let entry = LogBroadcast {
            id: 0, // DB assigns the real id asynchronously
            ts,
            level,
            target: meta.target().to_string(),
            message,
            fields,
            pipeline_id,
            output_id,
            event_type,
        };

        // send() fails only when all receivers have dropped — ignore the error.
        let _ = self.tx.send(entry);
    }
}

// ── Stderr writer (error/warn → stderr, rest → stdout) ───────────────────────

struct SplitWriter;

impl<'a> MakeWriter<'a> for SplitWriter {
    type Writer = Box<dyn std::io::Write + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Box::new(std::io::stdout())
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        if *meta.level() <= tracing::Level::WARN {
            Box::new(std::io::stderr())
        } else {
            Box::new(std::io::stdout())
        }
    }
}

// ── init() ───────────────────────────────────────────────────────────────────

/// Initialise the global tracing subscriber. Must be called once before any
/// tracing macros are used.
///
/// `db_pool` is used by the background drain task that batch-writes to app_logs.
pub fn init(db_pool: SqlitePool) -> LoggingHandles {
    // ── channel plumbing ──
    let (db_tx, mut db_rx) = tokio::sync::mpsc::channel::<AppLogEntry>(4096);
    let (broadcast_tx, _) = broadcast::channel::<LogBroadcast>(256);

    // ── span stores — shared between DbLayer and BroadcastLayer ──
    let db_spans = SpanStore::default();
    let bc_spans = SpanStore::default();

    // ── file sink ──
    let log_dir = std::env::var("RESTREAM_LOG_DIR").unwrap_or_else(|_| "logs".to_string());
    let (file_writer, file_guard) = if log_dir.is_empty() {
        // Disabled — write to a sink that discards everything.
        tracing_appender::non_blocking(std::io::sink())
    } else {
        let appender = tracing_appender::rolling::daily(&log_dir, "restream.log");
        tracing_appender::non_blocking(appender)
    };

    // ── RUST_LOG / EnvFilter ──
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // ── assemble subscriber ──
    let db_layer = DbLayer {
        tx: db_tx.clone(),
        spans: db_spans,
    };
    let bc_layer = BroadcastLayer {
        tx: broadcast_tx.clone(),
        spans: bc_spans,
    };

    let fmt_layer_stdout = tracing_subscriber::fmt::layer()
        .with_writer(SplitWriter)
        .with_target(true)
        .with_thread_ids(false)
        .with_ansi(std::env::var("NO_COLOR").is_err());

    let fmt_layer_file = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true)
        .json();

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer_stdout)
        .with(fmt_layer_file)
        .with(db_layer)
        .with(bc_layer);

    // set_global_default panics if called twice — acceptable for a process-lifetime init.
    tracing::subscriber::set_global_default(subscriber)
        .expect("tracing subscriber already installed");

    // ── DB drain task — spawned here, runs until db_tx is dropped ──
    tokio::spawn(async move {
        let mut batch: Vec<AppLogEntry> = Vec::with_capacity(64);
        loop {
            // Collect up to 64 entries or wait 100 ms, whichever comes first.
            let deadline = tokio::time::sleep(Duration::from_millis(100));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    biased;
                    entry = db_rx.recv() => {
                        match entry {
                            Some(e) => {
                                batch.push(e);
                                if batch.len() >= 64 { break; }
                            }
                            None => {
                                // Channel closed — flush remaining and exit.
                                if !batch.is_empty() {
                                    let _ = crate::db::append_app_log_batch(&db_pool, &batch).await;
                                }
                                return;
                            }
                        }
                    }
                    _ = &mut deadline => break,
                }
            }
            if !batch.is_empty() {
                let _ = crate::db::append_app_log_batch(&db_pool, &batch).await;
                batch.clear();
            }
        }
    });

    LoggingHandles {
        broadcast_tx,
        _db_tx: db_tx,
        _file_guard: file_guard,
    }
}
