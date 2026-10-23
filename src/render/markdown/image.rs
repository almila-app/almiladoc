use std::borrow::Cow;

use crate::ImagePolicy;
use crate::diagnostics::DiagnosticCode;
use crate::model::{AssetId, ImageSource};
use crate::render::markdown::escape::controls_to_spaces;
use crate::shared::uri::is_safe_link;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Presentation<'a> {
    Omit,
    Label,
    Marker,
    Asset(AssetId),
    Link(&'a str),
}

pub(super) struct Projection<'a> {
    pub(super) presentation: Presentation<'a>,
    pub(super) loss: DiagnosticCode,
}

pub(super) fn project(policy: ImagePolicy, source: &ImageSource, in_label: bool) -> Projection<'_> {
    let (presentation, loss) = match policy {
        ImagePolicy::Omit => (Presentation::Omit, DiagnosticCode::ImageOmitted),
        ImagePolicy::AltText => (Presentation::Label, DiagnosticCode::ImageOmitted),
        ImagePolicy::Placeholder => match source {
            ImageSource::Asset(id) => (Presentation::Asset(*id), DiagnosticCode::ImagePlaceholder),
            ImageSource::Unavailable => (Presentation::Marker, DiagnosticCode::ImagePlaceholder),
            ImageSource::Unread => (Presentation::Marker, DiagnosticCode::ImageUnavailable),
            ImageSource::External(_) if in_label => {
                (Presentation::Label, DiagnosticCode::ImageOmitted)
            }
            ImageSource::External(url) if !is_safe_link(url) => {
                (Presentation::Label, DiagnosticCode::UnsafeLink)
            }
            ImageSource::External(url) => (Presentation::Link(url), DiagnosticCode::ImageOmitted),
        },
    };
    Projection { presentation, loss }
}

pub(super) fn label(alt: &str) -> Cow<'_, str> {
    match alt.trim() {
        "" => Cow::Borrowed("Image"),
        alt => controls_to_spaces(alt),
    }
}
