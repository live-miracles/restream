//! Loom model-checks for TS chunk wait/cancel coordination.
//! This file owns the synchronization contract behind
//! `TsChunkReader::wait_for_data_or_cancelled`, ensuring wakeups and
//! cancellation race safely without deadlock or invalid states.

#[cfg(loom)]
mod loom_tests {
    use loom::sync::{Arc, Condvar, Mutex};
    use loom::thread;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum WaitResult {
        Data,
        Cancelled,
    }

    struct FakeTsChunkWait {
        mu: Mutex<State>,
        cvar: Condvar,
    }

    #[derive(Clone, Copy)]
    struct State {
        data_ready: bool,
        cancelled: bool,
    }

    impl FakeTsChunkWait {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                mu: Mutex::new(State {
                    data_ready: false,
                    cancelled: false,
                }),
                cvar: Condvar::new(),
            })
        }

        fn push_data(&self) {
            let mut guard = self.mu.lock().unwrap();
            guard.data_ready = true;
            self.cvar.notify_all();
        }

        fn cancel(&self) {
            let mut guard = self.mu.lock().unwrap();
            guard.cancelled = true;
            self.cvar.notify_all();
        }

        fn wait_for_data_or_cancelled(&self) -> WaitResult {
            let mut guard = self.mu.lock().unwrap();
            loop {
                if guard.cancelled {
                    return WaitResult::Cancelled;
                }
                if guard.data_ready {
                    return WaitResult::Data;
                }
                guard = self.cvar.wait(guard).unwrap();
            }
        }
    }

    #[test]
    fn loom_cancel_wakes_blocked_ts_reader() {
        loom::model(|| {
            let wait = FakeTsChunkWait::new();
            let reader_wait = wait.clone();

            let reader = thread::spawn(move || {
                let result = reader_wait.wait_for_data_or_cancelled();
                assert_eq!(result, WaitResult::Cancelled);
            });

            wait.cancel();
            reader.join().unwrap();
        });
    }

    #[test]
    fn loom_data_wakes_blocked_ts_reader() {
        loom::model(|| {
            let wait = FakeTsChunkWait::new();
            let reader_wait = wait.clone();

            let reader = thread::spawn(move || {
                let result = reader_wait.wait_for_data_or_cancelled();
                assert_eq!(result, WaitResult::Data);
            });

            wait.push_data();
            reader.join().unwrap();
        });
    }

    #[test]
    fn loom_ts_reader_race_between_data_and_cancel_still_completes() {
        loom::model(|| {
            let wait = FakeTsChunkWait::new();
            let reader_wait = wait.clone();
            let data_wait = wait.clone();
            let cancel_wait = wait.clone();

            let reader = thread::spawn(move || {
                let result = reader_wait.wait_for_data_or_cancelled();
                assert!(
                    matches!(result, WaitResult::Data | WaitResult::Cancelled),
                    "reader must wake with a valid outcome when data and cancel race"
                );
            });

            let data = thread::spawn(move || data_wait.push_data());
            let cancel = thread::spawn(move || cancel_wait.cancel());

            data.join().unwrap();
            cancel.join().unwrap();
            reader.join().unwrap();
        });
    }
}
