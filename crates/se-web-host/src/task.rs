//! Run Rust closures on CEF threads.

use cef::*;
use parking_lot::Mutex;
use std::sync::Arc;

type Job = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

wrap_task! {
    struct FnTask {
        job: Job,
    }

    impl Task {
        fn execute(&self) {
            let job = self.job.lock().take();
            if let Some(f) = job {
                f();
            }
        }
    }
}

fn task(f: impl FnOnce() + Send + 'static) -> Task {
    FnTask::new(Arc::new(Mutex::new(Some(Box::new(f)))))
}

/// Run `f` on the browser UI thread. `false` if CEF is shutting down.
pub fn on_ui(f: impl FnOnce() + Send + 'static) -> bool {
    post_task(ThreadId::UI, Some(&mut task(f))) == 1
}

/// Run `f` on the browser UI thread after `ms` milliseconds.
pub fn on_ui_after(ms: u64, f: impl FnOnce() + Send + 'static) -> bool {
    post_delayed_task(ThreadId::UI, Some(&mut task(f)), ms as i64) == 1
}
