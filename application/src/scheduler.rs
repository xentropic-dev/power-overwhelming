use anyhow::Result;
use std::time::Duration;

/// Work to run once, later.
pub type Job = Box<dyn FnOnce() + Send>;

/// Runs deferred work. An implementation holds a bounded amount of it, and says so
/// by refusing a job instead of dropping one it accepted.
pub trait Scheduler: Send + Sync {
    /// Runs `job` once `delay` has passed. An error means the job will never run,
    /// so the caller still owns that work.
    fn schedule(&self, delay: Duration, job: Job) -> Result<()>;
}
