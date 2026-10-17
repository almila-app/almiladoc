//! What a [`Converter`](crate::Converter) is built from, and why it may refuse it.

use crate::diagnostics::OnLoss;
use crate::limits::{Limit, Limits};

/// The configuration of one [`Converter`](crate::Converter), checked by
/// [`Converter::new`](crate::Converter::new).
///
/// # Examples
///
/// Set fields on the default.
///
/// ```
/// use almiladoc::{Config, ConvertError, Converter, Format, Input, Limit};
///
/// let mut config = Config::default();
/// config.threads = 4;
/// config.limits.max_input_bytes = 4;
/// let converter = Converter::new(config)?;
///
/// let csv = b"name,year\nAda,1843\n";
/// let refused = converter.convert(Input::new(csv, Format::Csv));
/// assert!(matches!(
///     refused,
///     Err(ConvertError::ResourceLimit { limit: Limit::InputBytes, .. })
/// ));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// A struct literal is refused outside the crate:
///
/// ```compile_fail,E0639
/// use almiladoc::Config;
///
/// let config = Config { threads: 4, ..Config::default() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// The safety limits every call enforces.
    pub limits: Limits,
    /// Threads each call runs on, from 1 to [`Config::MAX_THREADS`]; only PDF
    /// extraction works on more than one.
    pub threads: usize,
    /// What a conversion does once some content was lost.
    pub on_loss: OnLoss,
}

impl Config {
    /// The most threads one call may run on.
    pub const MAX_THREADS: usize = 64;

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=Self::MAX_THREADS).contains(&self.threads) {
            return Err(ConfigError::Threads { max: Self::MAX_THREADS });
        }
        for (limit, value) in self.limits.fields() {
            if value == 0 {
                return Err(ConfigError::ZeroLimit { limit });
            }
            let ceiling = Limits::ceiling(limit);
            if value > ceiling {
                return Err(ConfigError::AboveCeiling { limit, ceiling });
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Config { limits: Limits::default(), threads: 1, on_loss: OnLoss::Fail }
    }
}

/// Why a [`Config`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A limit is zero, which no input could meet.
    #[error("{limit} must not be zero")]
    #[non_exhaustive]
    ZeroLimit {
        /// The limit set to zero.
        limit: Limit,
    },
    /// A limit is above the highest value it accepts.
    #[error("{limit} must not exceed {ceiling}")]
    #[non_exhaustive]
    AboveCeiling {
        /// The limit set too high.
        limit: Limit,
        /// The highest value it accepts.
        ceiling: u64,
    },
    /// `threads` is zero or above its maximum.
    #[error("threads must be between 1 and {max}")]
    #[non_exhaustive]
    Threads {
        /// [`Config::MAX_THREADS`].
        max: usize,
    },
}

const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<ConfigError>();

#[cfg(test)]
mod tests {
    use super::*;

    fn with_threads(threads: usize) -> Config {
        Config { threads, ..Config::default() }
    }

    #[test]
    fn the_default_is_valid_and_single_threaded() {
        let config = Config::default();
        assert_eq!(config.threads, 1);
        assert_eq!(config.limits, Limits::default());
        assert_eq!(config.on_loss, OnLoss::Fail);
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn threads_run_from_one_to_the_maximum() {
        for threads in [1, 3, Config::MAX_THREADS] {
            assert_eq!(with_threads(threads).validate(), Ok(()), "{threads}");
        }
        for threads in [0, Config::MAX_THREADS + 1] {
            assert_eq!(
                with_threads(threads).validate(),
                Err(ConfigError::Threads { max: 64 }),
                "{threads}"
            );
        }
    }

    #[test]
    fn every_zero_limit_is_named() {
        let fields = Limits::default().fields();
        for (index, (limit, _)) in fields.iter().enumerate() {
            let mut config = Config::default();
            set(&mut config.limits, index, 0);
            assert_eq!(config.validate(), Err(ConfigError::ZeroLimit { limit: *limit }));
            set(&mut config.limits, index, 1);
            assert_eq!(config.validate(), Ok(()), "{limit} at 1");
        }
    }

    #[test]
    fn nesting_depth_stops_at_the_ceiling() {
        let mut config = Config::default();
        config.limits.max_nesting_depth = Limits::MAX_NESTING_CEILING;
        assert_eq!(config.validate(), Ok(()));
        config.limits.max_nesting_depth = Limits::MAX_NESTING_CEILING + 1;
        assert_eq!(
            config.validate(),
            Err(ConfigError::AboveCeiling { limit: Limit::NestingDepth, ceiling: 1024 })
        );
        let mut config = Config::default();
        config.limits.max_work = u64::MAX;
        config.limits.max_output_bytes = u64::MAX;
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn messages_name_the_field() {
        let cases = [
            (ConfigError::ZeroLimit { limit: Limit::Work }, "max_work must not be zero"),
            (
                ConfigError::AboveCeiling { limit: Limit::NestingDepth, ceiling: 1024 },
                "max_nesting_depth must not exceed 1024",
            ),
            (ConfigError::Threads { max: 64 }, "threads must be between 1 and 64"),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }

    fn set(limits: &mut Limits, index: usize, value: u64) {
        let field = match index {
            0 => &mut limits.max_entry_bytes,
            1 => &mut limits.max_total_bytes,
            2 => &mut limits.max_entry_count,
            3 => &mut limits.max_nesting_depth,
            4 => &mut limits.max_xml_nodes,
            5 => &mut limits.max_grid_slots,
            6 => &mut limits.max_expansion,
            7 => &mut limits.max_expansion_text_bytes,
            8 => &mut limits.max_asset_total_bytes,
            9 => &mut limits.max_record_depth,
            10 => &mut limits.max_records,
            11 => &mut limits.max_input_bytes,
            12 => &mut limits.max_compression_ratio,
            13 => &mut limits.max_output_bytes,
            14 => &mut limits.max_work,
            _ => unreachable!("Limits has 15 fields"),
        };
        *field = value;
    }
}
