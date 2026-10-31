use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::artifact::VerifiedModels;
use crate::inference::Pipeline;
use crate::runtime::quiet;
use crate::{Config, Error, ModelFiles, Page, Runtime, decode};

/// A single-image CPU pipeline with two sessions and no implicit model or runtime download.
pub struct OfflineOcr {
    _runtime: Runtime,
    engine: Pipeline,
    workers: ThreadPool,
    config: Config,
}

impl std::fmt::Debug for OfflineOcr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineOcr").field("config", &self.config).finish_non_exhaustive()
    }
}

impl OfflineOcr {
    /// Verify supplied model bytes and construct two sequential CPU sessions on a private pool.
    ///
    /// # Errors
    /// Refuses invalid configuration, unreadable or mismatched files, and pool or model initialization failures.
    pub fn new(runtime: &Runtime, models: &ModelFiles, config: Config) -> Result<Self, Error> {
        config.validate()?;
        let VerifiedModels { detector, recognizer, dictionary } = models.read(&config.limits)?;
        let workers = workers(config.threads)?;
        let engine =
            workers.install(|| Pipeline::new(detector, recognizer, dictionary, &config.limits))?;
        Ok(Self { _runtime: runtime.clone(), engine, workers, config })
    }

    /// Decode one bounded image against white and return literal text regions without layout inference.
    /// No detections gives an empty page; a detected region with no recognized text refuses the page.
    ///
    /// # Errors
    /// Refuses unsupported, malformed or oversized images and failed, invalid or oversized OCR output.
    pub fn recognize(&mut self, bytes: &[u8]) -> Result<Page, Error> {
        let image = quiet(|| decode::decode(bytes, &self.config.limits))?;
        self.recognize_image(image, &self.config.limits, &|| false)
    }

    pub(crate) fn recognize_image(
        &self,
        image: image::RgbImage,
        limits: &crate::Limits,
        cancelled: &(impl Fn() -> bool + Sync),
    ) -> Result<Page, Error> {
        self.workers.install(|| self.engine.recognize(image, limits, cancelled))
    }
}

fn workers(threads: usize) -> Result<ThreadPool, Error> {
    ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("ocr-local-{index}"))
        .spawn_handler(|worker| {
            std::thread::Builder::new()
                .name(worker.name().unwrap_or("ocr-local").to_owned())
                .spawn(move || quiet(|| worker.run()))
                .map(|_| ())
        })
        .build()
        .map_err(|_| Error::Pool)
}

#[cfg(test)]
mod tests {
    use super::workers;

    const TARGET: &str = "almiladoc_ocr_local::test::worker_privacy";

    fn emit() {
        tracing::info!(target: TARGET, privacy_probe = true);
    }

    #[test]
    fn private_workers_do_not_forward_events_to_the_global_subscriber() {
        let events = crate::test_logging::install(|metadata| metadata.target() == TARGET);
        emit();
        assert_eq!(events.count(), 1);
        let workers = workers(1).expect("private worker pool failed");
        workers.install(|| {
            assert_eq!(rayon::current_thread_index(), Some(0));
            emit();
        });
        assert_eq!(events.count(), 1);
        events.watch(None);
    }
}
