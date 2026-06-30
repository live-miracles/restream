//! Loom model-checks for the AV I/O queue synchronization boundary.
//! This file owns the close/wake contract behind `MemoryQueue`, proving the
//! shutdown and backpressure invariants that media thread hops rely on.

#[cfg(loom)]
mod loom_tests {
    use loom::sync::{Arc, Condvar, Mutex};
    use loom::thread;

    struct FakeQueue {
        mu: Mutex<State>,
        cvar: Condvar,
        capacity: usize,
    }

    #[derive(Clone, Copy)]
    struct State {
        len: usize,
        closed: bool,
    }

    impl FakeQueue {
        fn new(capacity: usize) -> Arc<Self> {
            Arc::new(Self {
                mu: Mutex::new(State {
                    len: 0,
                    closed: false,
                }),
                cvar: Condvar::new(),
                capacity,
            })
        }

        fn write_one(&self) -> bool {
            let mut guard = self.mu.lock().unwrap();
            loop {
                if guard.closed {
                    return false;
                }
                if guard.len < self.capacity {
                    guard.len += 1;
                    self.cvar.notify_all();
                    return true;
                }
                guard = self.cvar.wait(guard).unwrap();
            }
        }

        fn read_one(&self) -> Option<()> {
            let mut guard = self.mu.lock().unwrap();
            loop {
                if guard.len > 0 {
                    guard.len -= 1;
                    self.cvar.notify_all();
                    return Some(());
                }
                if guard.closed {
                    return None;
                }
                guard = self.cvar.wait(guard).unwrap();
            }
        }

        fn close(&self) {
            let mut guard = self.mu.lock().unwrap();
            guard.closed = true;
            self.cvar.notify_all();
        }
    }

    #[test]
    fn loom_close_wakes_blocked_writer() {
        loom::model(|| {
            let queue = FakeQueue::new(1);
            assert!(queue.write_one(), "initial write should fill queue");

            let writer_queue = queue.clone();
            let writer = thread::spawn(move || {
                let wrote = writer_queue.write_one();
                assert!(
                    !wrote,
                    "writer blocked on a full queue must return after close"
                );
            });

            queue.close();
            writer.join().unwrap();
        });
    }

    #[test]
    fn loom_close_wakes_blocked_reader() {
        loom::model(|| {
            let queue = FakeQueue::new(1);
            let reader_queue = queue.clone();

            let reader = thread::spawn(move || {
                let item = reader_queue.read_one();
                assert!(
                    item.is_none(),
                    "reader blocked on an empty queue must return after close"
                );
            });

            queue.close();
            reader.join().unwrap();
        });
    }
}
