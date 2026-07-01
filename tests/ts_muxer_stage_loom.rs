//! Loom model-checks for shared TS muxer-stage replacement.
//! This file owns the registry concurrency guarantees that prevent cancelled
//! muxer stages from being reused or duplicated under concurrent creation.

#[cfg(loom)]
mod loom_tests {
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    #[derive(Debug)]
    struct FakeStage {
        id: usize,
        cancelled: AtomicBool,
        reader_count: AtomicUsize,
    }

    impl FakeStage {
        fn new(id: usize) -> Arc<Self> {
            Arc::new(Self {
                id,
                cancelled: AtomicBool::new(false),
                reader_count: AtomicUsize::new(0),
            })
        }

        fn cancelled(id: usize) -> Arc<Self> {
            Arc::new(Self {
                id,
                cancelled: AtomicBool::new(true),
                reader_count: AtomicUsize::new(0),
            })
        }

        fn register_reader(&self) {
            self.reader_count.fetch_add(1, Ordering::AcqRel);
        }

        fn has_readers(&self) -> bool {
            self.reader_count.load(Ordering::Acquire) > 0
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

        fn sweep_unused(&self) {
            let mut guard = self.slot.lock().unwrap();
            let remove = guard.as_ref().is_some_and(|stage| !stage.has_readers());
            if remove {
                if let Some(stage) = guard.as_ref() {
                    stage.cancelled.store(true, Ordering::Release);
                }
                *guard = None;
            }
        }

        fn current_id(&self) -> Option<usize> {
            self.slot.lock().unwrap().as_ref().map(|stage| stage.id)
        }
    }

    #[test]
    fn loom_cancelled_stage_is_replaced_not_reused() {
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
    fn loom_concurrent_creators_share_one_replacement_after_cancelled_stage() {
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
                "concurrent creators must converge on one replacement stage"
            );
            assert_ne!(stage1.id, cancelled.id);
            assert!(!stage1.cancelled.load(Ordering::Acquire));
            assert_eq!(registry.current_id(), Some(stage1.id));
        });
    }

    #[test]
    fn loom_sweep_racing_reader_registration_is_serialized() {
        loom::model(|| {
            let stage = FakeStage::new(1);
            let registry = FakeRegistry::new(Some(stage.clone()));
            let sweep_registry = registry.clone();
            let reader_stage = stage.clone();

            let sweep = thread::spawn(move || sweep_registry.sweep_unused());
            let reader = thread::spawn(move || reader_stage.register_reader());

            sweep.join().unwrap();
            reader.join().unwrap();

            match registry.slot.lock().unwrap().as_ref() {
                Some(current) => {
                    assert!(Arc::ptr_eq(current, &stage));
                    assert!(!current.cancelled.load(Ordering::Acquire));
                    assert!(current.has_readers());
                }
                None => {
                    assert!(stage.cancelled.load(Ordering::Acquire));
                    assert!(stage.has_readers());
                }
            }
        });
    }
}
