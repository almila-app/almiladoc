//! What a conversion's output depends on beyond its input: the engine's
//! version and the configuration that shapes the output.

use sha2::{Digest, Sha256};

use crate::config::Config;

/// The version of almiladoc and the fingerprint of the configuration that
/// together fix a conversion's output.
///
/// Under the committed `Cargo.lock`, equal identities and equal input give
/// byte-identical output across threads, calls and, except for PDF,
/// architectures.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct EngineIdentity {
    /// The crate version.
    pub version: &'static str,
    /// The lowercase hex SHA-256 of `key=value;` pairs, one per output-shaping [`Config`]
    /// field in declaration order, omitting default header/footer policy for compatibility,
    /// and leaving out `limits` and `threads`, which
    /// never change output that converts.
    pub config_fingerprint: String,
}

impl EngineIdentity {
    pub(crate) fn of(config: &Config) -> Self {
        let digest = Sha256::digest(fingerprinted(config).as_bytes());
        let mut config_fingerprint = String::new();
        config_fingerprint.extend(digest.iter().map(|byte| format!("{byte:02x}")));
        Self { version: env!("CARGO_PKG_VERSION"), config_fingerprint }
    }
}

fn fingerprinted(config: &Config) -> String {
    let Config { limits: _, threads: _, on_loss, images, image_ocr, headers_footers } = config;
    let mut fields = format!(
        "on_loss={};images={};image_ocr={};",
        on_loss.as_str(),
        images.as_str(),
        image_ocr.as_str()
    );
    match headers_footers {
        crate::HeaderFooterPolicy::Omit => {}
        crate::HeaderFooterPolicy::Emit => fields.push_str("headers_footers=emit;"),
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::OnLoss;

    #[test]
    fn the_default_fingerprint_is_pinned() {
        let identity = EngineIdentity::of(&Config::default());
        assert_eq!(identity.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            identity.config_fingerprint,
            "d1a451c320c6528d18924f27421c5855c58af1f0cd72c42cb6e77732f81e90fc"
        );
    }

    #[test]
    fn output_shaping_fields_change_it_and_resource_fields_do_not() {
        let fail = EngineIdentity::of(&Config::default());
        let report = EngineIdentity::of(&Config { on_loss: OnLoss::Report, ..Config::default() });
        assert_ne!(fail, report);
        assert_eq!(
            fingerprinted(&Config { on_loss: OnLoss::Report, ..Config::default() }),
            "on_loss=report;images=placeholder;image_ocr=none;"
        );
        for images in [crate::ImagePolicy::AltText, crate::ImagePolicy::Omit] {
            assert_ne!(EngineIdentity::of(&Config { images, ..Config::default() }), fail);
        }
        assert_ne!(
            EngineIdentity::of(&Config {
                headers_footers: crate::HeaderFooterPolicy::Emit,
                ..Config::default()
            }),
            fail
        );
        let image_ocr = crate::ImageOcr::All;
        assert_ne!(EngineIdentity::of(&Config { image_ocr, ..Config::default() }), fail);
        let mut resourced = Config { threads: 8, ..Config::default() };
        resourced.limits.max_work = 1;
        assert_eq!(EngineIdentity::of(&resourced), fail);
    }
}
