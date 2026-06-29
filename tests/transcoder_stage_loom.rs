/// Loom model-check for shared H.265->H.264 transcoder stage replacement.
///
/// The production `get_or_create_h264_transcoder()` path stores one shared
/// codec-edge stage per `(pipeline, upstream)` key in `stages.buffers`, guarded
/// by a write lock plus a cancellation token. The invariants we need from that
/// synchronization boundary are:
///
/// 1. A cancelled stage must never be reused.
/// 2. Concurrent creators must converge on one replacement stage.
/// 3. The registry must publish at most one live stage for the key.

#[cfg(loom)]
mod loom_tests {
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    #[derive(Debug)]
    struct FakeStage {
        id: usize,
        cancelled: AtomicBool,
    }

    impl FakeStage {
        fn new(id: usize) -> Arc<Self> {
            Arc::new(Self {
                id,
                cancelled: AtomicBool::new(false),
            })
        }

        fn cancelled(id: usize) -> Arc<Self> {
            Arc::new(Self {
                id,
                cancelled: AtomicBool::new(true),
            })
        }
    }

    struct FakeRegistry {
        slot: Mutex<Option<Arc<FakeStage>>>,
        next_id: AtomicUsize,
    }

    impl FakeRegistry {
        fn new(initial: Option<Arc<FakeStage>>) -> Arc<Self> {
            let next_id = initial
                .as_ref()
                .map(|stage| stage.id.saturating_add(1))
                .unwrap_or(1);
            Arc::new(Self {
                slot: Mutex::new(initial),
                next_id: AtomicUsize::new(next_id),
            })
        }

        fn get_or_create(&self) -> Arc<FakeStage> {
            let mut guard = self.slot.lock().unwrap();
            if let Some(stage) = guard.as_ref()
                && !stage.cancelled.load(Ordering::Acquire)
            {
                return stage.clone();
            }

            let id = self.next_id.fetch_add(1, Ordering::AcqRel);
            let stage = FakeStage::new(id);
            *guard = Some(stage.clone());
            stage
        }

        fn cancel_and_remove(&self) {
            let mut guard = self.slot.lock().unwrap();
            if let Some(stage) = guard.as_ref() {
                stage.cancelled.store(true, Ordering::Release);
            }
            *guard = None;
        }

        fn current_id(&self) -> Option<usize> {
            self.slot.lock().unwrap().as_ref().map(|stage| stage.id)
        }
    }

    #[test]
    fn loom_cancelled_codec_edge_stage_is_replaced_not_reused() {
        loom::model(|| {
            let initial = FakeStage::new(1);
            let registry = FakeRegistry::new(Some(initial.clone()));

            registry.cancel_and_remove();
            let replacement = registry.get_or_create();

            assert_ne!(replacement.id, initial.id);
            assert!(!replacement.cancelled.load(Ordering::Acquire));
            assert_eq!(registry.current_id(), Some(replacement.id));
        });
    }

    #[test]
    fn loom_concurrent_creators_share_one_codec_edge_replacement() {
        loom::model(|| {
            let cancelled = FakeStage::cancelled(1);
            let registry = FakeRegistry::new(Some(cancelled.clone()));
            let creator_a = registry.clone();
            let creator_b = registry.clone();

            let t1 = thread::spawn(move || creator_a.get_or_create());
            let t2 = thread::spawn(move || creator_b.get_or_create());

            let stage1 = t1.join().unwrap();
            let stage2 = t2.join().unwrap();

            assert!(
                Arc::ptr_eq(&stage1, &stage2),
                "concurrent creators must converge on one replacement codec-edge stage"
            );
            assert_ne!(stage1.id, cancelled.id);
            assert!(!stage1.cancelled.load(Ordering::Acquire));
            assert_eq!(registry.current_id(), Some(stage1.id));
        });
    }
}
