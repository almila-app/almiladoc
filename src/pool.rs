//! The per-call thread pool every walking call runs on.
//!
//! Each call builds its own pool of `Config::threads` threads, so no work,
//! the PDF engine's parallel iterators included, reaches rayon's global pool
//! or the caller's stack.

use std::panic::{AssertUnwindSafe, catch_unwind};

use rayon::{ThreadPool, ThreadPoolBuildError, ThreadPoolBuilder};

use crate::config::Config;
use crate::error::ConvertError;

/// Stack of each pool thread: the measured stack cost of one level of the
/// deepest recursion over input nesting, times
/// [`Limits::MAX_NESTING_CEILING`](crate::Limits::MAX_NESTING_CEILING). A
/// stack overflow aborts the process, so this is never sized below that
/// product.
const STACK: usize = 8 * 1024 * 1024;

/// Run `work` on a pool built for `config`, mapping a pool that cannot start
/// and a panic in `work` to [`ConvertError::Internal`].
pub(crate) fn run_on_pool<T: Send>(
    config: &Config,
    work: impl FnOnce() -> Result<T, ConvertError> + Send,
) -> Result<T, ConvertError> {
    run_in(ThreadPoolBuilder::new().num_threads(config.threads).stack_size(STACK).build(), work)
}

fn run_in<T: Send>(
    pool: Result<ThreadPool, ThreadPoolBuildError>,
    work: impl FnOnce() -> Result<T, ConvertError> + Send,
) -> Result<T, ConvertError> {
    let pool = pool.map_err(|_| ConvertError::Internal)?;
    pool.install(|| catch_unwind(AssertUnwindSafe(work)).unwrap_or(Err(ConvertError::Internal)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threads(threads: usize) -> Config {
        Config { threads, ..Config::default() }
    }

    #[test]
    fn work_runs_on_a_pool_of_the_configured_threads() {
        for count in [1, 3] {
            let seen = run_on_pool(&threads(count), || Ok(rayon::current_num_threads()));
            assert_eq!(seen.ok(), Some(count));
        }
    }

    #[test]
    fn work_runs_off_the_callers_thread() {
        let caller = std::thread::current().id();
        let worker = run_on_pool(&threads(1), || Ok(std::thread::current().id()));
        assert!(worker.is_ok_and(|worker| worker != caller));
    }

    #[test]
    fn work_runs_on_the_sized_stack() {
        let used = run_on_pool(&threads(1), || Ok(fill_stack()));
        assert_eq!(used.ok(), Some(6 * 1024 * 1024));
    }

    #[inline(never)]
    fn fill_stack() -> usize {
        let mut frame = [0u8; 6 * 1024 * 1024];
        std::hint::black_box(&mut frame);
        frame.len()
    }

    #[test]
    fn results_and_errors_pass_through() {
        assert_eq!(run_on_pool(&threads(1), || Ok(7)).ok(), Some(7));
        let refused = run_on_pool(&threads(1), || Err::<(), _>(ConvertError::Encrypted));
        assert!(matches!(refused, Err(ConvertError::Encrypted)));
    }

    #[test]
    fn a_panic_is_an_internal_error() {
        let result = run_on_pool(&threads(2), || -> Result<(), ConvertError> {
            panic!("a defect");
        });
        assert!(matches!(result, Err(ConvertError::Internal)), "{result:?}");
    }

    #[test]
    fn a_panic_on_another_pool_thread_is_an_internal_error() {
        let result = run_on_pool(&threads(2), || {
            rayon::join(|| (), || panic!("a defect"));
            Ok(())
        });
        assert!(matches!(result, Err(ConvertError::Internal)), "{result:?}");
    }

    #[test]
    fn a_pool_that_cannot_start_is_an_internal_error() {
        let pool = ThreadPoolBuilder::new()
            .num_threads(1)
            .spawn_handler(|_| Err(std::io::Error::other("no threads")))
            .build();
        assert!(pool.is_err());
        let result = run_in(pool, || Ok(()));
        assert!(matches!(result, Err(ConvertError::Internal)), "{result:?}");
    }
}
