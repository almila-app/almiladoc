//! The per-call thread pool every walking call runs on.
//!
//! Each call builds its own pool of `Config::threads` threads, so no work,
//! the PDF engine's parallel iterators included, reaches rayon's global pool
//! or the caller's stack.

use std::panic::{AssertUnwindSafe, catch_unwind};

use rayon::{ThreadPool, ThreadPoolBuildError, ThreadPoolBuilder};

use crate::config::Config;
use crate::error::ConvertError;

/// Stack of each pool thread: the costliest recursion over input nesting at
/// [`Limits::MAX_NESTING_CEILING`](crate::Limits::MAX_NESTING_CEILING), parse
/// and render, times 1.5, rounded up to 64 KiB. A stack overflow aborts the
/// process, so this is never sized below it.
///
/// Measured as the smallest stack converting each recursion's input at two
/// depths, in bytes per level of the limit and in the test profile (the
/// release profile in brackets); the fixed base is at most 37 KiB:
///
/// | Input | Recursions | Per level |
/// |---|---|---|
/// | EPUB nested `a`, in a paragraph or a heading | HTML inlines, rendered links, heading text | 7238 (1443) |
/// | EPUB nested `a` and `b` | HTML inlines, rendered links | 6449 (1068) |
/// | EPUB nested `blockquote` | HTML blocks, rendered quotes | 4593 (938) |
/// | EPUB nested `div` | HTML blocks | 4016 (705) |
/// | EPUB nested `ul` | HTML lists, rendered lists | 3936 (472) |
/// | DOCX nested `w:hyperlink`, with or without a target | DOCX inlines, rendered links | 2835 (450) |
/// | DOCX nested `w:smartTag` | DOCX inlines | 2835 (450) |
/// | ODT nested tables | ODF tables, rendered tables | 2562 (562) |
/// | ODT nested `text:span` | ODF inlines | 2498 (305) |
/// | ODT nested `text:a`, in a paragraph or a table cell | ODF inlines, cell text bytes, rendered links | 2498 (451) |
/// | ODP nested `draw:g` | ODF shapes | 2446 (306) |
/// | EPUB MathML nested `mrow` | MathML | 2321 (353) |
/// | ODT nested lists | ODF lists, rendered lists | 1928 (345) |
/// | DOC nested HYPERLINK fields | rendered links | 1664 (448) |
/// | DOCX nested tables | DOCX blocks, rendered tables | 1614 (434) |
/// | PPTX nested `p:grpSp` | PPTX shapes | 992 (718) |
/// | DOCX nested `mc:AlternateContent` in a run | DOCX run content | 859 (96) |
/// | ODT nested `table:table-row-group` | ODF table rows | 845 (129) |
/// | RTF nested HYPERLINK fields | rendered links | 832 (224) |
/// | DOCX OMML nested `m:f` | OMML | 747 (169) |
/// | RTF nested `\mf` | OMML | 739 (169) |
/// | DOCX nested `v:group` | DOCX drawing contents | 386 (185) |
/// | DOCX nested `w:customXml` around cells | DOCX row cells | 306 (113) |
/// | RTF nested groups | none | 0 (0) |
const STACK: usize = 11_206_656;

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
        assert_eq!(used.ok(), Some(STACK - 1024 * 1024));
    }

    #[inline(never)]
    fn fill_stack() -> usize {
        let mut frame = [0u8; STACK - 1024 * 1024];
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
