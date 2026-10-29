//! The thread a query is parsed and evaluated on, and the timer that cancels it.

use std::panic::resume_unwind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use rift_error::{RiftError, errors};
use spareval::CancellationToken;

/// Stack bytes of the query thread, 256 MiB of address space, committed only as touched. The
/// parser, the optimizer and the evaluator recurse at most once per opening bracket and once per
/// operator character, and the structure bounds cap both counts. A debug build overflowed a
/// 64 MiB stack parsing a chain of 4,000 `+1` terms;
/// `test_graph_query_parse_survives_brackets_and_operators_at_their_bounds` parses and
/// evaluates both bounds together, nested, on this stack.
pub(super) const QUERY_STACK_BYTES: usize = 256 * 1024 * 1024;

/// The deadline one evaluation is held to.
pub(super) struct Watch<'a> {
    /// Time before the token is cancelled.
    pub(super) timeout: Duration,
    /// The token the evaluator checks.
    pub(super) token: &'a CancellationToken,
    /// Set when the timer cancelled the token.
    pub(super) expired: &'a AtomicBool,
}

/// Runs `work` on a scoped thread with [`QUERY_STACK_BYTES`] of stack and returns its result.
///
/// With a `watch`, the calling thread waits for `work` at most `timeout`, then cancels the token
/// and waits for `work` to observe it: spareval checks the token at every dataset read and inside
/// its join loops. A panic in `work` resumes on the caller.
///
/// # Errors
///
/// Returns `query_evaluation_failed` when the thread cannot be started.
pub(super) fn run<T: Send>(
    watch: Option<Watch<'_>>,
    work: impl FnOnce() -> T + Send,
) -> Result<T, RiftError> {
    thread::scope(|scope| {
        let (finished, finish) = mpsc::sync_channel::<()>(1);
        let worker = thread::Builder::new()
            .name("rift-sparql".to_owned())
            .stack_size(QUERY_STACK_BYTES)
            .spawn_scoped(scope, move || {
                let result = work();
                // The caller may have stopped waiting; a closed channel changes nothing.
                let _ = finished.send(());
                result
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                return errors::search::query_evaluation_failed()
                    .detail("the query thread did not start")
                    .source(error)
                    .fail();
            }
        };
        if let Some(watch) = watch
            && finish.recv_timeout(watch.timeout) == Err(RecvTimeoutError::Timeout)
        {
            watch.expired.store(true, Ordering::Relaxed);
            watch.token.cancel();
        }
        worker.join().map_err(|panic| resume_unwind(panic))
    })
}

#[cfg(test)]
mod tests {
    use super::{Watch, run};
    use spareval::CancellationToken;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn test_query_thread_timer_cancels_work_that_never_reads() {
        let token = CancellationToken::new();
        let expired = AtomicBool::new(false);
        let watch = Watch {
            timeout: Duration::from_millis(20),
            token: &token,
            expired: &expired,
        };
        let spins = run(Some(watch), || {
            let mut spins = 0_u64;
            while !token.is_cancelled() {
                spins += 1;
                std::hint::spin_loop();
            }
            spins
        })
        .expect("the thread starts");
        assert!(
            spins > 0 && expired.load(Ordering::Relaxed),
            "the timer cancels the work"
        );
        let quick = run(None, || 7).expect("the thread starts");
        assert_eq!(quick, 7);
    }
}
