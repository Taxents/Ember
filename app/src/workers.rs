//! Below-normal-priority worker threads: a few for API calls and a larger pool for artwork, so
//! pictures never wait behind a page load (or the other way round). Last in, first out: what
//! the user just asked for (the page they opened, the rows on screen) runs before older
//! requests. Urgent jobs (what holds up the banner) run before all of them.

use std::sync::{Arc, Condvar, Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Jobs {
    urgent: Vec<Job>,
    normal: Vec<Job>,
}

struct Pool {
    jobs: Mutex<Jobs>,
    ready: Condvar,
}

impl Pool {
    fn start(name: &'static str, threads: usize) -> Arc<Pool> {
        let q = Arc::new(Pool {
            jobs: Mutex::new(Jobs::default()),
            ready: Condvar::new(),
        });
        for i in 0..threads {
            let q = q.clone();
            std::thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || {
                    crate::win::below_normal_priority();
                    loop {
                        let job = {
                            let mut jobs = q.jobs.lock().unwrap();
                            loop {
                                if let Some(j) = jobs.urgent.pop().or_else(|| jobs.normal.pop()) {
                                    break j;
                                }
                                jobs = q.ready.wait(jobs).unwrap();
                            }
                        };
                        job();
                    }
                })
                .expect("spawn worker");
        }
        q
    }

    fn push(&self, job: Job, urgent: bool) {
        let mut jobs = self.jobs.lock().unwrap();
        if urgent {
            jobs.urgent.push(job);
        } else {
            jobs.normal.push(job);
        }
        drop(jobs);
        self.ready.notify_one();
    }
}

fn api() -> &'static Pool {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    POOL.get_or_init(|| Pool::start("net", 3))
}

fn art() -> &'static Pool {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    // Mostly waiting on the network (images come from a few hosts, over kept-alive connections).
    POOL.get_or_init(|| Pool::start("art", 8))
}

fn background() -> &'static Pool {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    // Look-ups nobody waits for (play counts per row): never in the way of a page or a click.
    POOL.get_or_init(|| Pool::start("bg", 2))
}

/// [`run`], on threads of their own, for the many small look-ups a page asks for.
pub fn run_background<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + Send + 'static,
) {
    background().push(
        Box::new(move || {
            let r = job();
            let _ = slint::invoke_from_event_loop(move || done(r));
        }),
        false,
    );
}

/// Runs `job` on a worker thread.
pub fn spawn(job: impl FnOnce() + Send + 'static) {
    api().push(Box::new(job), false);
}

/// Runs `job` on a worker thread before anything already waiting.
pub fn spawn_urgent(job: impl FnOnce() + Send + 'static) {
    api().push(Box::new(job), true);
}

/// Loads artwork on the artwork pool, `urgent` ahead of what's waiting.
pub fn spawn_art(job: impl FnOnce() + Send + 'static, urgent: bool) {
    art().push(Box::new(job), urgent);
}

/// Runs `job` on a worker, then `done` with its result on the UI thread.
pub fn run<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + Send + 'static,
) {
    spawn(move || {
        let r = job();
        let _ = slint::invoke_from_event_loop(move || done(r));
    });
}

/// [`run`], ahead of the queue.
pub fn run_urgent<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + Send + 'static,
) {
    spawn_urgent(move || {
        let r = job();
        let _ = slint::invoke_from_event_loop(move || done(r));
    });
}
