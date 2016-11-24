#[allow(unused_imports)]
use latch::{Latch, SpinLatch, UpgradeLatch};
use job::{Job, StackJob, JobResult, JobRef, JobMode};
use std::mem;
use std::sync::Arc;
use std::thread;
use thread_pool::{self, WorkerThread};

/// Fires off a task into the Rayon threadpool that will run
/// asynchronously. This this task runs asynchronously, it cannot hold
/// any references to the enclosing stack frame. Like a regular Rust
/// thread, it should always be created with a `move` closure and
/// should not hold any references (except for `&'static` references).
///
/// NB: If this closure should panic, the resulting error is just
/// dropped onto the floor and is not propagated.
///
/// # Examples
///
/// This code creates a Rayon that task. The task fires off a message
/// on the channel (`22`) when it executes. The spawning task then
/// waits for the message. This is a handy pattern if you want to
/// delegate work into the Rayon threadpool.
///
/// **Warning: Do not write blocking code like this on the Rayon
/// threadpool!** This is only useful if you know that you are not
/// currently in the Rayon threadpool; otherwise, it will be
/// inefficient at best and could deadlock at worst.
///
/// ```rust
/// use std::sync::mpsc::channel;
///
/// // Create a channel
/// let (tx, rx) = channel();
///
/// // Start an async rayon thread, giving it the
/// // transmit endpoint.
/// rayon::spawn_async(move || {
///     tx.send(22).unwrap();
/// });
///
/// // Block until the Rayon thread sends us some data.
/// // Note that if the job should panic (or otherwise terminate)
/// // before sending, this `unwrap()` will fail.
/// let v = rx.recv().unwrap();
/// assert!(v == 22);
/// ```
pub fn spawn_async<F, R>(func: F) -> SpawnAsync<F, R>
    where F: FnOnce() -> R + Send + 'static
{
    let async_job = Arc::new(AsyncJob::new(func));

    unsafe {
        // We assert that this does not hold any references (we know
        // this because of the `'static` bound in the inferface);
        // moreover, we assert that the code below is not supposed to
        // be able to panic, and hence the data won't leak but will be
        // enqueued into some deque for later execution.
        let job_ref = AsyncJob::as_job_ref(async_job.clone());
        let worker_thread = WorkerThread::current();
        if worker_thread.is_null() {
            thread_pool::get_registry().inject(&[job_ref]);
        } else {
            (*worker_thread).push(job_ref);
        }
    }

    // We assert that `async_job` is the only handle, other than the
    // one enqueded into the Rayon thread-pool.
    unsafe { SpawnAsync::new(async_job) }
}

/// Represents a job that has been spawned.
struct AsyncJob<F, R>
    where F: FnOnce() -> R + Send + 'static
{
    stack_job: StackJob<UpgradeLatch, F, R>,
}

unsafe impl<F, R> Send for AsyncJob<F, R>
    where F: FnOnce() -> R + Send + 'static
{ }

unsafe impl<F, R> Sync for AsyncJob<F, R>
    where F: FnOnce() -> R + Send + 'static
{ }

impl<F, R> AsyncJob<F, R>
    where F: FnOnce() -> R + Send + 'static
{
    pub fn new(func: F) -> Self {
        AsyncJob { stack_job: StackJob::new(func, UpgradeLatch::new()) }
    }

    /// Creates a `JobRef` from this job -- note that this hides all
    /// lifetimes, so it is up to you to ensure that this JobRef
    /// doesn't outlive any data that it closes over.
    pub unsafe fn as_job_ref(this: Arc<Self>) -> JobRef {
        let this: *const Self = mem::transmute(this);
        JobRef::new(this)
    }

    pub fn latch(&self) -> &UpgradeLatch {
        &self.stack_job.latch
    }

    pub fn take_result(&mut self) -> JobResult<R> {
        self.stack_job.take_result()
    }
}

impl<F, R> Job for AsyncJob<F, R>
    where F: FnOnce() -> R + Send + 'static
{
    unsafe fn execute(this: *const Self, mode: JobMode) {
        let this: Arc<Self> = mem::transmute(this);

        // We assert that the stack-job will outlive the call to
        // `execute`.
        Job::execute(&this.stack_job, mode);
    }
}

///
pub struct SpawnAsync<F, R>
    where F: FnOnce() -> R + Send + 'static
{
    /// NB. We do not implement clone on `SpawnAsync`, so we know that
    /// this is the only handle to the async-job.
    job: Option<Arc<AsyncJob<F, R>>>,
}

impl<F, R> SpawnAsync<F, R>
    where F: FnOnce() -> R + Send + 'static
{
    /// Safety: must be the only handle to `job`, except for the one
    /// enqueued into the Rayon scheduler.
    unsafe fn new(job: Arc<AsyncJob<F, R>>) -> Self {
        SpawnAsync { job: Some(job) }
    }

    pub fn poll(&mut self) -> Option<R> {
        let done = self.job.as_ref().map(|job| job.latch().probe()).unwrap_or(false);
        if !done {
            return None;
        }

        let mut job = self.job.take().unwrap();

        // Once the job is done, the other handle to the arc will be
        // dropped shortly thereafter. But it may not have happened
        // *yet*, so we spin briefly waiting for it to happen. Note
        // that this spin will never be for long, since the other CPU
        // must be doing the drop.
        loop {
            if let Some(job_ref) = Arc::get_mut(&mut job) {
                let result = job_ref.take_result();
                return Some(result.into_return_value());
            } else {
                thread::yield_now();
            }
        }
    }

    /// Blocks until the job is done. Once the job is complete,
    /// returns its result. Returns `None` if you have called this already.
    pub fn join(&mut self) -> Option<R> {
        if let Some(ref job) = self.job {
            unsafe {
                let owner_thread = WorkerThread::current();
                if !owner_thread.is_null() {
                    (*owner_thread).steal_until(job.latch());
                } else {
                    job.latch().wait();
                }
            }
        }

        self.poll()
    }
}

#[cfg(test)]
mod test;
