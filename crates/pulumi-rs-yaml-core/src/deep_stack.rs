// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! The one thread the engine's deep work runs on.
//!
//! A caller's stack is whatever its thread was given: 8 MiB on a Linux main
//! thread, 2 MiB on a spawned or tokio thread, 1 MiB on a Windows main
//! thread. Recursion bounded by a count -- the SQL parser's nesting, a
//! template's macro and include depth -- needs a stack sized for that count,
//! not for the caller, and under `panic = "abort"` running out is a process
//! abort rather than an error. So that work runs here: on a worker with a
//! [`DEEP_STACK_BYTES`] stack, reserved rather than committed, so the pages a
//! shallow job never touches cost address space only.
//!
//! One worker per calling thread, parked between jobs. Spawning per call
//! measured 31.5us (with the 64 MiB reservation), and a twenty-view program
//! makes about forty SQL calls; reusing the worker removes that churn.
//! Thread-local rather than threaded through a context: the call sites keep
//! their signatures, an idle worker costs no CPU, and dropping the sender
//! when the calling thread ends closes the worker's loop.
//!
//! The jobs are a closed enum rather than boxed closures: work crosses a
//! channel, so it must be owned and `Send`, and listing it keeps the
//! isolation contract legible -- exactly what crosses is named here.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{channel, Receiver, Sender};

/// Stack for the worker that runs recursive work.
pub(crate) const DEEP_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Work that may recurse deeper than a caller's stack allows.
pub(crate) enum Job {
    #[cfg(feature = "sql-lineage")]
    Sql(crate::sql_lineage::Job),
    /// Boxed: a render carries its whole context, and the SQL jobs are small.
    Render(Box<crate::jinja::RenderJob>),
}

/// The answer to a [`Job`], of the matching variant.
pub(crate) enum Reply {
    #[cfg(feature = "sql-lineage")]
    Sql(crate::sql_lineage::Reply),
    Render(crate::jinja::RenderReply),
}

impl Job {
    fn execute(self) -> Reply {
        match self {
            #[cfg(feature = "sql-lineage")]
            Job::Sql(job) => Reply::Sql(job.execute()),
            Job::Render(job) => Reply::Render(job.execute()),
        }
    }
}

struct Worker {
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
    /// The worker's thread, so reuse can be observed rather than assumed.
    #[cfg_attr(not(test), allow(dead_code))]
    id: std::thread::ThreadId,
}

impl Worker {
    fn spawn() -> Result<Self, String> {
        let (jobs, job_rx) = channel::<Job>();
        let (reply_tx, replies) = channel::<Reply>();
        SPAWNED.with(|n| n.set(n.get() + 1));
        let handle = std::thread::Builder::new()
            .stack_size(DEEP_STACK_BYTES)
            .name("deep-stack".to_string())
            .spawn(move || {
                ON_WORKER.with(|w| w.set(true));
                // Ends when the sender is dropped, i.e. when the calling
                // thread goes away.
                while let Ok(job) = job_rx.recv() {
                    if reply_tx.send(job.execute()).is_err() {
                        break; // the caller is gone
                    }
                }
            })
            .map_err(|e| format!("failed to start the deep-stack worker: {e}"))?;
        Ok(Worker {
            jobs,
            replies,
            id: handle.thread().id(),
        })
    }

    fn run(&self, job: Job) -> Result<Reply, String> {
        const GONE: &str = "the deep-stack worker terminated unexpectedly";
        self.jobs.send(job).map_err(|_| GONE.to_string())?;
        self.replies.recv().map_err(|_| GONE.to_string())
    }
}

thread_local! {
    /// Set on a worker's own thread: work arriving there already has the
    /// deep stack, and runs where it is.
    static ON_WORKER: Cell<bool> = const { Cell::new(false) };
    /// Workers this thread has started: one for its life, unless one died.
    static SPAWNED: Cell<usize> = const { Cell::new(0) };
    /// `None` until first use, and `None` again after a worker died -- work
    /// that cannot reach a worker is reported, never run on the caller's
    /// stack instead.
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

/// Runs `job` on this thread's worker, starting it on first use.
pub(crate) fn run(job: Job) -> Result<Reply, String> {
    if ON_WORKER.with(Cell::get) {
        return Ok(job.execute());
    }
    WORKER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Worker::spawn()?);
        }
        let out = slot.as_ref().expect("just filled").run(job);
        if out.is_err() {
            // A dead worker must not poison every later call on this thread.
            *slot = None;
        }
        out
    })
}

/// How many workers this thread has started.
#[cfg(test)]
pub(crate) fn spawned_on_this_thread() -> usize {
    SPAWNED.with(Cell::get)
}

/// The worker parked for this thread, if any.
#[cfg(test)]
pub(crate) fn current_worker() -> Option<std::thread::ThreadId> {
    WORKER.with(|c| c.borrow().as_ref().map(|w| w.id))
}

/// Parks a worker that is already gone, as one that died would be.
#[cfg(test)]
pub(crate) fn install_dead_worker() -> std::thread::ThreadId {
    let (jobs, rx) = channel::<Job>();
    drop(rx); // every send fails
    let (_tx, replies) = channel::<Reply>();
    let id = std::thread::current().id();
    WORKER.with(|c| *c.borrow_mut() = Some(Worker { jobs, replies, id }));
    id
}

#[cfg(test)]
mod tests {
    use crate::jinja::{JinjaContext, JinjaLimits, JinjaPreprocessor, UndefinedMode};
    use std::collections::HashMap;

    /// Counted, not timed: a thread starts its worker once, for its first
    /// render that can recurse, and never again; a render that cannot
    /// recurse starts none.
    #[test]
    fn a_render_starts_no_thread_of_its_own() {
        std::thread::spawn(|| {
            let (config, extra) = (HashMap::new(), HashMap::new());
            let ctx = JinjaContext {
                project_name: "p",
                stack_name: "s",
                cwd: ".",
                organization: "",
                root_directory: ".",
                config: &config,
                project_dir: ".",
                undefined: UndefinedMode::Strict,
                provider_templated_packages: &[],
                extra: &extra,
                limits: JinjaLimits::default(),
            };
            let pre = JinjaPreprocessor::new(&ctx);
            for _ in 0..50 {
                assert_eq!(pre.render("a: {{ 1 + 1 }}", "P.yaml").unwrap(), "a: 2");
            }
            assert_eq!(
                super::spawned_on_this_thread(),
                0,
                "plain renders run inline"
            );
            let deep = "{% macro m(n) %}{{ n }}{% endmacro %}a: {{ m(3) }}";
            for _ in 0..50 {
                assert_eq!(pre.render(deep, "P.yaml").unwrap(), "a: 3");
            }
            assert_eq!(super::spawned_on_this_thread(), 1, "one worker, reused");
            #[cfg(feature = "sql-lineage")]
            {
                assert!(crate::sql_lineage::analyze_select("SELECT a FROM `p.d.t`").is_ok());
                assert_eq!(super::spawned_on_this_thread(), 1, "SQL parsing shares it");
            }
        })
        .join()
        .unwrap();
    }
}
