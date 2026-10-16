//! A thread that runs blocking jobs off the UI thread, one at a time, in the order they
//! were queued. Session logs and provider startup each use one.

use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

type Job = Box<dyn FnOnce() + Send>;

pub struct Worker {
    jobs: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn new(name: &str) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            })
            .expect("spawn worker thread");
        Self {
            jobs: Some(jobs),
            thread: Some(thread),
        }
    }

    /// Queues `job` after every job queued before it.
    pub fn run(&self, job: impl FnOnce() + Send + 'static) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Box::new(job));
        }
    }

    /// Blocks until every job queued before this call has finished.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        self.run(move || {
            let _ = done.send(());
        });
        let _ = wait.recv();
    }
}

impl Drop for Worker {
    /// Finishes the queued jobs before returning, so nothing queued is lost.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn jobs_run_in_order_and_flush_waits_for_them() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let worker = Worker::new("test-worker");
        for n in 0..100 {
            let seen = seen.clone();
            worker.run(move || seen.lock().unwrap().push(n));
        }
        worker.flush();
        assert_eq!(*seen.lock().unwrap(), (0..100).collect::<Vec<_>>());

        let seen2 = seen.clone();
        worker.run(move || {
            thread::sleep(std::time::Duration::from_millis(50));
            seen2.lock().unwrap().push(100);
        });
        drop(worker);
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&100),
            "drop finishes queued jobs"
        );
    }
}
