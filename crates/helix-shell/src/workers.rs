//! A few worker threads for work the UI thread must not wait for (icon
//! decoding, blurring and scaling pictures): jobs run in the order given,
//! and each reports back on its own.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    jobs: Mutex<(VecDeque<Job>, bool)>,
    ready: Condvar,
}

/// The threads (started when the first job comes) and their queue.
/// Dropping it lets the threads finish their current job and stop.
pub struct Workers {
    name: &'static str,
    threads: usize,
    started: usize,
    queue: Arc<Queue>,
}

impl Workers {
    /// Up to `threads` threads named `name-N`.
    pub fn new(name: &'static str, threads: usize) -> Self {
        Self { name, threads: threads.max(1), started: 0, queue: Arc::default() }
    }

    /// A pool sized for the machine: a quarter of its threads, 1 to 4.
    pub fn sized(name: &'static str) -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        Self::new(name, (cores / 4).clamp(1, 4))
    }

    /// Run `job` on a worker.
    pub fn run(&mut self, job: impl FnOnce() + Send + 'static) {
        let waiting = {
            let mut jobs = self.queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.0.push_back(Box::new(job));
            jobs.0.len()
        };
        // Another thread while there is a backlog and room for one.
        if self.started < self.threads && (self.started == 0 || waiting > 1) {
            let queue = self.queue.clone();
            let spawned = std::thread::Builder::new().name(format!("{}-{}", self.name, self.started)).spawn(move || work(&queue));
            match spawned {
                Ok(_) => self.started += 1,
                Err(err) => tracing::warn!(%err, name = self.name, "cannot start a worker thread"),
            }
        }
        if self.started == 0 {
            // No thread at all: run here rather than never.
            let job = self.queue.jobs.lock().unwrap_or_else(|e| e.into_inner()).0.pop_back();
            if let Some(job) = job {
                job();
            }
            return;
        }
        self.queue.ready.notify_one();
    }

    /// Drop the jobs not started yet.
    pub fn cancel_queued(&mut self) {
        self.queue.jobs.lock().unwrap_or_else(|e| e.into_inner()).0.clear();
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        let mut jobs = self.queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.0.clear();
        jobs.1 = true;
        drop(jobs);
        self.queue.ready.notify_all();
    }
}

fn work(queue: &Queue) {
    loop {
        let job = {
            let mut jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if jobs.1 {
                    return;
                }
                if let Some(job) = jobs.0.pop_front() {
                    break job;
                }
                jobs = queue.ready.wait(jobs).unwrap_or_else(|e| e.into_inner());
            }
        };
        // A job that panics takes only itself down.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
            tracing::warn!("a background job panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn jobs_run_off_this_thread() {
        let mut workers = Workers::new("test", 2);
        let (tx, rx) = mpsc::channel();
        let here = std::thread::current().id();
        for i in 0..8 {
            let tx = tx.clone();
            workers.run(move || {
                let _ = tx.send((i, std::thread::current().id() != here));
            });
        }
        let mut got: Vec<(i32, bool)> = (0..8).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        got.sort();
        assert_eq!(got, (0..8).map(|i| (i, true)).collect::<Vec<_>>());
        assert!(workers.started <= 2);
    }

    #[test]
    fn a_panicking_job_does_not_stop_the_pool() {
        let mut workers = Workers::new("test", 1);
        let (tx, rx) = mpsc::channel();
        workers.run(|| panic!("boom"));
        workers.run(move || {
            let _ = tx.send(1);
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(1));
    }
}
