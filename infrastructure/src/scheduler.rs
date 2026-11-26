//! A single timer thread that runs deferred jobs, holding at most a fixed number.

use anyhow::{Context, Result, ensure};
use application::{Job, Scheduler};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

/// How many jobs a scheduler holds before it refuses more.
pub const DEFAULT_CAPACITY: usize = 4096;

#[derive(Default)]
struct Queue {
    due: BinaryHeap<Reverse<(Instant, u64)>>,
    jobs: BTreeMap<u64, Job>,
    next: u64,
    /// Set when the timer thread is gone, so no accepted job could ever run.
    closed: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
}

/// Runs jobs on one thread started up front, so accepting a job never needs a new
/// thread. A job that panics is logged and the thread carries on.
pub struct TimerScheduler {
    shared: Arc<Shared>,
    capacity: usize,
}

impl TimerScheduler {
    pub fn start(capacity: usize) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let timer = shared.clone();
        thread::Builder::new()
            .name("power-scheduler".into())
            .spawn(move || run(&timer))
            .context("Could not start the scheduler thread")?;
        Ok(Self { shared, capacity })
    }
}

impl Scheduler for TimerScheduler {
    fn schedule(&self, delay: Duration, job: Job) -> Result<()> {
        let mut queue = self.shared.queue.lock().unwrap();
        ensure!(!queue.closed, "The scheduler thread has stopped");
        ensure!(
            queue.jobs.len() < self.capacity,
            "The scheduler already holds {} jobs",
            self.capacity
        );
        let id = queue.next;
        queue.next += 1;
        queue.due.push(Reverse((Instant::now() + delay, id)));
        queue.jobs.insert(id, job);
        self.shared.changed.notify_all();
        Ok(())
    }
}

/// Marks the queue closed if the timer thread ever unwinds.
struct Closing<'a>(&'a Shared);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.0.queue.lock() {
            queue.closed = true;
        }
    }
}

fn run(shared: &Shared) {
    let _closing = Closing(shared);
    let mut queue = shared.queue.lock().unwrap();
    loop {
        let now = Instant::now();
        match queue.due.peek().copied() {
            Some(Reverse((at, id))) if at <= now => {
                queue.due.pop();
                let Some(job) = queue.jobs.remove(&id) else {
                    continue;
                };
                drop(queue);
                if catch_unwind(AssertUnwindSafe(job)).is_err() {
                    eprintln!("A scheduled job panicked.");
                }
                queue = shared.queue.lock().unwrap();
            }
            Some(Reverse((at, _))) => {
                queue = shared.changed.wait_timeout(queue, at - now).unwrap().0;
            }
            None => queue = shared.changed.wait(queue).unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn jobs_run_in_due_order_after_their_delay() -> Result<()> {
        let scheduler = TimerScheduler::start(8)?;
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        for (delay, name) in [(150, "late"), (50, "early")] {
            let tx = tx.clone();
            scheduler.schedule(
                Duration::from_millis(delay),
                Box::new(move || tx.send((name, started.elapsed())).unwrap()),
            )?;
        }
        let first = rx.recv_timeout(Duration::from_secs(5))?;
        let second = rx.recv_timeout(Duration::from_secs(5))?;
        assert_eq!((first.0, second.0), ("early", "late"));
        assert!(first.1 >= Duration::from_millis(50));
        assert!(second.1 >= Duration::from_millis(150));
        Ok(())
    }

    #[test]
    fn a_full_scheduler_refuses_jobs_instead_of_dropping_them() -> Result<()> {
        let scheduler = TimerScheduler::start(2)?;
        let (tx, rx) = mpsc::channel();
        for _ in 0..2 {
            let tx = tx.clone();
            scheduler.schedule(
                Duration::from_millis(200),
                Box::new(move || tx.send(()).unwrap()),
            )?;
        }
        assert!(scheduler.schedule(Duration::ZERO, Box::new(|| {})).is_err());
        // Both accepted jobs still run.
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5))?;
        }
        Ok(())
    }

    #[test]
    fn a_panicking_job_does_not_stop_later_ones() -> Result<()> {
        let scheduler = TimerScheduler::start(4)?;
        scheduler.schedule(Duration::ZERO, Box::new(|| panic!("job failed")))?;
        let (tx, rx) = mpsc::channel();
        scheduler.schedule(
            Duration::from_millis(20),
            Box::new(move || tx.send(()).unwrap()),
        )?;
        rx.recv_timeout(Duration::from_secs(5))?;
        Ok(())
    }
}
