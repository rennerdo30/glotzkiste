//! A small fixed pool for blocking jobs (HTTP, image decode, file I/O).
//! Results come back through the caller's own channel plus a UI wake.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct Pool {
    tx: Sender<Job>,
}

impl Pool {
    pub fn new(threads: usize) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..threads.max(1) {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("gk-worker-{i}")).spawn(move || {
                loop {
                    let job = {
                        let Ok(guard) = rx.lock() else { return };
                        match guard.recv() {
                            Ok(j) => j,
                            Err(_) => return,
                        }
                    };
                    // A panicking job is reported; the worker survives.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                        log::error!("a background job panicked");
                    }
                }
            });
        }
        Self { tx }
    }

    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.tx.send(Box::new(job));
    }
}
