use std::sync::{Arc, Mutex};

use crate::artifact::read_verified;
use crate::{Error, FileRole, FileSpec, Limits};

static RUNTIME_STATE: Mutex<Initialization> = Mutex::new(Initialization::Uninitialized);

enum Initialization {
    Uninitialized,
    Failed,
    Ready([u8; 32]),
}

impl Initialization {
    fn initialize(
        &mut self,
        digest: [u8; 32],
        initialize: impl FnOnce() -> Result<(), Error>,
    ) -> Result<(), Error> {
        match self {
            Self::Ready(loaded) => {
                return if *loaded == digest { Ok(()) } else { Err(Error::RuntimeConflict) };
            }
            Self::Failed => return Err(Error::Runtime),
            Self::Uninitialized => {}
        }
        *self = Self::Failed;
        initialize()?;
        *self = Self::Ready(digest);
        Ok(())
    }
}

/// A handle to the explicitly initialized process-wide ONNX Runtime.
#[derive(Debug, Clone)]
pub struct Runtime {
    _private: (),
}

impl Runtime {
    /// Load a library supporting API 27 before other ORT use; keep its path and dependencies trusted.
    ///
    /// # Errors
    /// Refuses invalid limits, unreadable or mismatched files, load failures, and known runtime conflicts.
    /// Native initialization failures require a new process; file-verification errors can be corrected.
    pub fn load(file: &FileSpec, limits: &Limits) -> Result<Self, Error> {
        limits.validate()?;
        let path =
            file.path().canonicalize().map_err(|_| Error::FileUnreadable(FileRole::Runtime))?;
        let canonical = FileSpec::new(&path, file.digest());
        drop(read_verified(&canonical, FileRole::Runtime, limits.max_artifact_bytes)?);
        let mut state = RUNTIME_STATE.lock().map_err(|_| Error::Runtime)?;
        state.initialize(file.digest(), || {
            quiet(|| {
                let committed = ort::init_from(path)
                    .map_err(|_| Error::Runtime)?
                    .with_telemetry(false)
                    .with_logger(Arc::new(|_, _, _, _, _| {}))
                    .commit();
                if !committed {
                    return Err(Error::RuntimeConflict);
                }
                ort::environment::Environment::current().map_err(|_| Error::Runtime)?;
                Ok(())
            })
        })?;
        Ok(Self { _private: () })
    }

    #[cfg(test)]
    pub(crate) fn test_handle() -> Self {
        Self { _private: () }
    }
}

pub(crate) fn quiet<T>(operation: impl FnOnce() -> T) -> T {
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn failed_initialization_never_retries_the_native_initializer() {
        for failure in [Error::Runtime, Error::RuntimeConflict] {
            let mut state = Initialization::Uninitialized;
            let calls = Cell::new(0);
            assert!(
                state
                    .initialize([1; 32], || {
                        calls.set(calls.get() + 1);
                        Err(failure)
                    })
                    .is_err()
            );
            for digest in [[1; 32], [2; 32]] {
                assert!(matches!(
                    state.initialize(digest, || {
                        calls.set(calls.get() + 1);
                        Ok(())
                    }),
                    Err(Error::Runtime)
                ));
            }
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn initialized_runtime_reuses_only_its_verified_digest() {
        let mut state = Initialization::Uninitialized;
        let calls = Cell::new(0);
        state
            .initialize([1; 32], || {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap();
        for (digest, expected) in [([1; 32], Ok(())), ([2; 32], Err(Error::RuntimeConflict))] {
            assert_eq!(
                state.initialize(digest, || {
                    calls.set(calls.get() + 1);
                    Ok(())
                }),
                expected
            );
        }
        assert_eq!(calls.get(), 1);
    }
}
