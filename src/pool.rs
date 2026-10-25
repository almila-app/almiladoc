//! The per-call thread pool every walking call runs on.
//!
//! Each call builds its own pool of `Config::threads` threads, so no work
//! reaches rayon's global pool or the caller's stack.

use std::panic::{AssertUnwindSafe, catch_unwind};

use rayon::{ThreadPool, ThreadPoolBuildError};

use crate::config::Config;
use crate::error::ConvertError;

/// Stack of each pool thread: the costliest recursion over input nesting at
/// [`Limits::MAX_NESTING_CEILING`](crate::Limits::MAX_NESTING_CEILING), parse,
/// render or JSON, times 1.5, rounded up to 64 KiB. A stack overflow aborts the
/// process, so this is never sized below it.
///
/// Measured as the smallest stack converting each recursion's input at two
/// depths, in bytes per level of the limit and unoptimised (opt-level 0, the dev
/// profile; the release profile in brackets); the fixed base is at most 49 KiB:
///
/// | Input | Recursions | Per level |
/// |---|---|---|
/// | EPUB nested `a`, in a paragraph, a heading or a list item | HTML inlines, rendered links, heading text | 7390 (1357) |
/// | EPUB nested `a` and `b` | HTML inlines, rendered links | 6610 (988) |
/// | EPUB nested `blockquote` | HTML blocks, rendered quotes | 4753 (869) |
/// | A parsed document's tables nested in cells, its costliest nesting to serialise | JSON | 4656 (352) |
/// | ODT nested `text:a`, in a paragraph or a table cell | ODF inlines, cell text bytes, rendered links | 4611 (531) |
/// | DOCX nested `w:hyperlink`, with or without a target | DOCX inlines, rendered links | 4610 (530) |
/// | DOCX nested complex HYPERLINK fields, alone or alternating with `w:hyperlink` | DOCX inlines, rendered links | 4608 (528) |
/// | DOC nested HYPERLINK fields | rendered links | 4608 (528) |
/// | EPUB nested `div` | HTML blocks | 4176 (625) |
/// | EPUB nested `ul` | HTML lists, rendered lists | 4104 (648) |
/// | ODT nested lists | ODF lists, rendered lists | 3132 (297) |
/// | DOCX nested `w:smartTag` | DOCX inlines | 2835 (466) |
/// | ODT nested tables | ODF tables, rendered tables | 2707 (570) |
/// | ODT nested `text:span` | ODF inlines | 2490 (337) |
/// | ODP nested `draw:g` | ODF shapes | 2454 (306) |
/// | EPUB MathML nested `mrow` | MathML | 2321 (353) |
/// | RTF nested HYPERLINK fields | rendered links | 2304 (264) |
/// | DOCX complex HYPERLINK fields alternating with `w:hyperlink` of an unread relationship | DOCX inlines, plain text, rendered links | 2304 (264) |
/// | DOCX nested tables | DOCX blocks, rendered tables | 2128 (442) |
/// | PPTX nested `p:grpSp` | PPTX shapes | 992 (718) |
/// | DOCX nested `mc:AlternateContent` in a run | DOCX run content | 867 (96) |
/// | ODT nested `table:table-row-group` | ODF table rows | 845 (121) |
/// | DOCX OMML nested `m:f` | OMML | 739 (185) |
/// | RTF nested `\mf` | OMML | 739 (185) |
/// | DOCX nested `v:group` | DOCX drawing contents | 377 (185) |
/// | DOCX nested `w:customXml` around cells | DOCX row cells | 323 (113) |
/// | RTF nested groups | none | 0 (0) |
const STACK: usize = 11_468_800;

/// Run `work` on a pool built for `config`, mapping a pool that cannot start
/// and a panic in `work` to [`ConvertError::Internal`].
#[expect(
    clippy::disallowed_types,
    reason = "the one pool every walking call runs on is built here"
)]
pub(crate) fn run_on_pool<T: Send>(
    config: &Config,
    work: impl FnOnce() -> Result<T, ConvertError> + Send,
) -> Result<T, ConvertError> {
    run_in(
        rayon::ThreadPoolBuilder::new().num_threads(config.threads).stack_size(STACK).build(),
        work,
    )
}

/// `map` of each of `items`, in their order, up to the threads of the pool it is called in at
/// once: called only inside the work [`run_on_pool`] runs, so none reaches rayon's global pool.
#[expect(
    clippy::disallowed_methods,
    reason = "a batch's items fan out over the per-call pool its work runs on"
)]
pub(crate) fn map_ordered<T: Sync, R: Send>(
    items: &[T],
    map: impl Fn(&T) -> R + Sync + Send,
) -> Vec<R> {
    use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
    items.par_iter().map(map).collect()
}

#[cfg(feature = "ocr-http")]
pub(crate) use network::wait;

/// What a network engine's requests run on: the calling thread's waits, which a cancel ends.
#[cfg(feature = "ocr-http")]
mod network {
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::ledger::CancelToken;

    /// How often a wait looks at its token.
    const CANCEL_POLL: Duration = Duration::from_millis(25);

    /// Wait `delay` on the calling thread, unless `cancel` is cancelled first; whether it was not.
    pub(crate) fn wait(delay: Duration, cancel: &CancelToken) -> bool {
        let end = Instant::now() + delay;
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            thread::sleep(left.min(CANCEL_POLL));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_wait_ends_at_its_delay_or_at_a_cancel() {
            let cancel = CancelToken::new();
            let start = Instant::now();
            assert!(wait(Duration::from_millis(60), &cancel));
            assert!(start.elapsed() >= Duration::from_millis(60));
            cancel.cancel();
            let start = Instant::now();
            assert!(!wait(Duration::from_secs(30), &cancel));
            assert!(start.elapsed() < Duration::from_millis(100));
        }
    }
}

fn run_in<T: Send>(
    pool: Result<ThreadPool, ThreadPoolBuildError>,
    work: impl FnOnce() -> Result<T, ConvertError> + Send,
) -> Result<T, ConvertError> {
    let pool = pool.map_err(|_| ConvertError::Internal)?;
    pool.install(|| catch_unwind(AssertUnwindSafe(work)).unwrap_or(Err(ConvertError::Internal)))
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "the tests observe the pool's threads and build a pool that cannot start"
)]
mod tests {
    use super::*;
    use std::assert_matches;

    const FRAME: usize = 8 * 1024;
    const SPARE: usize = 1024 * 1024;

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
        let frames = (STACK - SPARE) / FRAME;
        let used = run_on_pool(&threads(1), || Ok(fill_stack(frames)));
        assert_eq!(used.ok(), Some(frames * FRAME));
    }

    #[inline(never)]
    fn fill_stack(frames: usize) -> usize {
        if frames == 0 {
            return 0;
        }
        let mut frame = [0u8; FRAME];
        std::hint::black_box(&mut frame);
        frame.len() + fill_stack(frames - 1)
    }

    #[test]
    fn mapped_items_keep_their_order_and_spread_over_the_pool() {
        let items: Vec<u32> = (0..64).collect();
        let mapped = run_on_pool(&threads(3), || {
            Ok(map_ordered(&items, |&n| {
                std::thread::sleep(std::time::Duration::from_millis(u64::from(n % 3)));
                (n * 2, rayon::current_thread_index())
            }))
        })
        .unwrap();
        let doubled: Vec<u32> = mapped.iter().map(|(n, _)| *n).collect();
        assert_eq!(doubled, (0..64).map(|n| n * 2).collect::<Vec<_>>());
        let workers: std::collections::BTreeSet<_> = mapped.iter().map(|(_, at)| *at).collect();
        assert!(workers.iter().all(Option::is_some) && workers.len() > 1, "{workers:?}");
    }

    #[test]
    fn results_and_errors_pass_through() {
        assert_eq!(run_on_pool(&threads(1), || Ok(7)).ok(), Some(7));
        let refused = run_on_pool(&threads(1), || Err::<(), _>(ConvertError::Encrypted));
        assert_matches!(refused, Err(ConvertError::Encrypted));
    }

    #[test]
    fn a_panic_is_an_internal_error() {
        let result = run_on_pool(&threads(2), || -> Result<(), ConvertError> {
            panic!("a defect");
        });
        assert_matches!(result, Err(ConvertError::Internal));
    }

    #[test]
    fn a_panic_on_another_pool_thread_is_an_internal_error() {
        let result = run_on_pool(&threads(2), || {
            rayon::join(|| (), || panic!("a defect"));
            Ok(())
        });
        assert_matches!(result, Err(ConvertError::Internal));
    }

    #[test]
    fn a_pool_that_cannot_start_is_an_internal_error() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .spawn_handler(|_| Err(std::io::Error::other("no threads")))
            .build();
        assert_matches!(pool, Err(_));
        let result = run_in(pool, || Ok(()));
        assert_matches!(result, Err(ConvertError::Internal));
    }
}
