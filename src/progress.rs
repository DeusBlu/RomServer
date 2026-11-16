//! Progress of a long operation, shared between the thread doing the work and whoever shows it.
//! Work that nobody watches passes `&Progress::default()`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub struct Progress {
    stage: Mutex<String>,
    done: AtomicUsize,
    total: AtomicUsize,
}

/// What a progress bar shows at one moment.
pub struct Snapshot {
    pub stage: String,
    pub done: usize,
    pub total: usize,
}

impl Progress {
    /// Starts a new stage of `total` steps (0 when the count isn't known).
    pub fn stage(&self, name: impl Into<String>, total: usize) {
        *self.stage.lock().unwrap_or_else(|e| e.into_inner()) = name.into();
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn step(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            stage: self.stage.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
        }
    }
}
