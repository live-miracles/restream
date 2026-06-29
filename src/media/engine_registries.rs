use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock as TokioRwLock;
use tokio_util::sync::CancellationToken;

use crate::domain::stage::StageKey;
use crate::events::EventLog;
use crate::media::avio::MemoryQueue;
use crate::media::engine::{
    ActiveEgress, ActiveIngest, HlsConsumers, ListenerSocketStats, RecentIngestOutcome,
};
use crate::media::hls::HlsStore;
use crate::media::pipe_metrics::PipeMetrics;
use crate::media::ring_buffer::RingBuffer;
use crate::media::stage_metrics::StageMetrics;
use crate::media::ts_chunk_ring::TsChunkRing;

pub type TranscoderBuffer = (Arc<RingBuffer>, CancellationToken);

pub struct IngestRegistry {
    pub pipelines: TokioRwLock<HashMap<String, Arc<RingBuffer>>>,
    pub cancel_tokens: TokioRwLock<HashMap<String, CancellationToken>>,
    pub active: TokioRwLock<HashMap<String, ActiveIngest>>,
    pub recent: TokioRwLock<HashMap<String, RecentIngestOutcome>>,
}

impl IngestRegistry {
    pub fn new() -> Self {
        Self {
            pipelines: TokioRwLock::new(HashMap::new()),
            cancel_tokens: TokioRwLock::new(HashMap::new()),
            active: TokioRwLock::new(HashMap::new()),
            recent: TokioRwLock::new(HashMap::new()),
        }
    }
}

pub struct EgressRegistry {
    pub cancel_tokens: TokioRwLock<HashMap<String, CancellationToken>>,
    pub active: TokioRwLock<HashMap<String, ActiveEgress>>,
    pub queues: TokioRwLock<HashMap<String, Arc<MemoryQueue>>>,
}

impl EgressRegistry {
    pub fn new() -> Self {
        Self {
            cancel_tokens: TokioRwLock::new(HashMap::new()),
            active: TokioRwLock::new(HashMap::new()),
            queues: TokioRwLock::new(HashMap::new()),
        }
    }
}

pub struct RecordingRegistry {
    pub cancel_tokens: TokioRwLock<HashMap<String, CancellationToken>>,
}

impl RecordingRegistry {
    pub fn new() -> Self {
        Self {
            cancel_tokens: TokioRwLock::new(HashMap::new()),
        }
    }
}

pub struct HlsRegistry {
    pub stores: TokioRwLock<HashMap<String, Arc<HlsStore>>>,
    pub consumers: TokioRwLock<HashMap<String, HlsConsumers>>,
}

impl HlsRegistry {
    pub fn new() -> Self {
        Self {
            stores: TokioRwLock::new(HashMap::new()),
            consumers: TokioRwLock::new(HashMap::new()),
        }
    }
}

pub struct FileIngestRegistry {
    pub children: TokioRwLock<HashMap<String, tokio::process::Child>>,
    pub active: TokioRwLock<HashSet<String>>,
}

impl FileIngestRegistry {
    pub fn new() -> Self {
        Self {
            children: TokioRwLock::new(HashMap::new()),
            active: TokioRwLock::new(HashSet::new()),
        }
    }
}

pub struct StageRegistry {
    pub buffers: TokioRwLock<HashMap<StageKey, TranscoderBuffer>>,
    pub metrics: TokioRwLock<HashMap<StageKey, Arc<StageMetrics>>>,
    pub input_queues: TokioRwLock<HashMap<StageKey, Arc<MemoryQueue>>>,
    pub pipe_metrics: TokioRwLock<HashMap<StageKey, Arc<PipeMetrics>>>,
    pub ts_muxers: TokioRwLock<HashMap<String, Arc<TsChunkRing>>>,
}

impl StageRegistry {
    pub fn new() -> Self {
        Self {
            buffers: TokioRwLock::new(HashMap::new()),
            metrics: TokioRwLock::new(HashMap::new()),
            input_queues: TokioRwLock::new(HashMap::new()),
            pipe_metrics: TokioRwLock::new(HashMap::new()),
            ts_muxers: TokioRwLock::new(HashMap::new()),
        }
    }
}

pub struct RuntimeInfra {
    pub listener_stats: Arc<ListenerSocketStats>,
    pub os_threads: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    pub sender_semaphore: Arc<tokio::sync::Semaphore>,
    pub diag_semaphores: TokioRwLock<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    pub event_log: Arc<EventLog>,
}

impl RuntimeInfra {
    pub fn new() -> Self {
        Self {
            listener_stats: Arc::new(ListenerSocketStats::default()),
            os_threads: std::sync::Mutex::new(Vec::new()),
            sender_semaphore: Arc::new(tokio::sync::Semaphore::new(512)),
            diag_semaphores: TokioRwLock::new(HashMap::new()),
            event_log: Arc::new(EventLog::new()),
        }
    }
}
