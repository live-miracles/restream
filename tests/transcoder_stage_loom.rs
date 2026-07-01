//! Loom model-checks for shared transcoder-stage replacement.
//! This file owns the registry invariants around cancelled-stage replacement
//! so concurrent creators converge on one live codec-edge stage per key.

#[cfg(loom)]
mod loom_tests {
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    #[derive(Debug)]
    struct FakeStage {
        id: usize,
        codec_hint: &'static str,
        cancelled: AtomicBool,
    }

    impl FakeStage {
        fn new(id: usize, codec_hint: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                codec_hint,
                cancelled: AtomicBool::new(false),
            })
        }

        fn cancelled(id: usize, codec_hint: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                codec_hint,
                cancelled: AtomicBool::new(true),
            })
        }
    }

    struct FakeRegistry {
        slot: Mutex<Option<Arc<FakeStage>>>,
        codec_hint: &'static str,
        next_id: AtomicUsize,
    }

    impl FakeRegistry {
        fn new(initial: Option<Arc<FakeStage>>, codec_hint: &'static str) -> Arc<Self> {
            let next_id = initial
                .as_ref()
                .map(|stage| stage.id.saturating_add(1))
                .unwrap_or(1);
            Arc::new(Self {
                slot: Mutex::new(initial),
                codec_hint,
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
            let stage = FakeStage::new(id, self.codec_hint);
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

        fn cleanup_pipeline(&self) {
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
            let initial = FakeStage::new(1, "h264");
            let registry = FakeRegistry::new(Some(initial.clone()), "h264");

            registry.cancel_and_remove();
            let replacement = registry.get_or_create();

            assert_ne!(replacement.id, initial.id);
            assert!(!replacement.cancelled.load(Ordering::Acquire));
            assert_eq!(replacement.codec_hint, "h264");
            assert_eq!(registry.current_id(), Some(replacement.id));
        });
    }

    #[test]
    fn loom_concurrent_creators_share_one_codec_edge_replacement() {
        loom::model(|| {
            let cancelled = FakeStage::cancelled(1, "h264");
            let registry = FakeRegistry::new(Some(cancelled.clone()), "h264");
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
            assert_eq!(stage1.codec_hint, "h264");
            assert_eq!(registry.current_id(), Some(stage1.id));
        });
    }

    #[test]
    fn loom_cleanup_and_replacement_are_atomic() {
        loom::model(|| {
            let initial = FakeStage::new(1, "h264");
            let registry = FakeRegistry::new(Some(initial.clone()), "h264");
            let cleanup_registry = registry.clone();
            let creator_registry = registry.clone();

            let cleanup = thread::spawn(move || cleanup_registry.cleanup_pipeline());
            let creator = thread::spawn(move || creator_registry.get_or_create());

            cleanup.join().unwrap();
            let created = creator.join().unwrap();

            assert!(initial.cancelled.load(Ordering::Acquire));

            match registry.slot.lock().unwrap().as_ref() {
                Some(current) => {
                    assert!(Arc::ptr_eq(current, &created));
                    assert_ne!(current.id, initial.id);
                    assert!(!current.cancelled.load(Ordering::Acquire));
                    assert_eq!(current.codec_hint, "h264");
                }
                None => {
                    assert!(created.cancelled.load(Ordering::Acquire));
                }
            }
        });
    }

    #[test]
    fn loom_replacement_preserves_codec_metadata_contract() {
        loom::model(|| {
            let initial = FakeStage::new(7, "hevc");
            let registry = FakeRegistry::new(Some(initial), "hevc");

            registry.cancel_and_remove();

            let creator_a = registry.clone();
            let creator_b = registry.clone();
            let t1 = thread::spawn(move || creator_a.get_or_create());
            let t2 = thread::spawn(move || creator_b.get_or_create());

            let stage_a = t1.join().unwrap();
            let stage_b = t2.join().unwrap();
            assert!(
                Arc::ptr_eq(&stage_a, &stage_b),
                "replacement must converge under concurrent creation"
            );
            assert_eq!(stage_a.codec_hint, "hevc");
            assert!(!stage_a.cancelled.load(Ordering::Acquire));
        });
    }
}
