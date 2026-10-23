//! A few below-normal-priority threads for API calls and images. Last in, first out: what the
//! user just asked for (the page they opened, the rows on screen) runs before older requests.

use std::sync::{Arc, Condvar, Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send>;

struct Queue {
    jobs: Mutex<Vec<Job>>,
    ready: Condvar,
}

static QUEUE: OnceLock<Arc<Queue>> = OnceLock::new();
const THREADS: usize = 3;

fn queue() -> &'static Arc<Queue> {
    QUEUE.get_or_init(|| {
        let q = Arc::new(Queue {
            jobs: Mutex::new(Vec::new()),
            ready: Condvar::new(),
        });
        for i in 0..THREADS {
            let q = q.clone();
            std::thread::Builder::new()
                .name(format!("net-{i}"))
                .spawn(move || {
                    crate::win::below_normal_priority();
                    loop {
                        let job = {
                            let mut jobs = q.jobs.lock().unwrap();
                            loop {
                                if let Some(j) = jobs.pop() {
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
    })
}

/// Runs `job` on a worker thread.
pub fn spawn(job: impl FnOnce() + Send + 'static) {
    let q = queue();
    q.jobs.lock().unwrap().push(Box::new(job));
    q.ready.notify_one();
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
