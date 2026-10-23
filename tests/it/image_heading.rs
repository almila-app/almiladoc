use crate::build::epub_bare;
use crate::support::configured;
use almiladoc::{Format, ImagePolicy, Input, OnLoss};

fn markdown(body: &str, policy: ImagePolicy) -> String {
    let converter = configured(|config| {
        config.images = policy;
        config.on_loss = OnLoss::Report;
    });
    let bytes = epub_bare(body);
    let parsed = converter.parse(Input::new(&bytes, Format::Epub)).unwrap();
    let result = converter.convert(Input::new(&bytes, Format::Epub)).unwrap();
    assert_eq!(result, parsed.render().unwrap());
    result.into_markdown()
}
#[test]
fn image_heading_link_url_fallback_determines_its_actual_slug() {
    let body = r##"<h1 id="target"><a href="https://example.org"><img src="missing.png" alt="Chart"/></a></h1><p><a href="#target">go</a></p>"##;
    for (policy, expected) in [
        (ImagePolicy::Placeholder, "# [\\[Chart\\]](https://example.org)\n\n[go](#chart)\n"),
        (ImagePolicy::AltText, "# [Chart](https://example.org)\n\n[go](#chart)\n"),
        (
            ImagePolicy::Omit,
            "# [https://example.org](https://example.org)\n\n[go](#httpsexampleorg)\n",
        ),
    ] {
        assert_eq!(markdown(body, policy), expected);
    }
}
#[test]
fn invisible_internal_image_heading_does_not_claim_a_slug() {
    let body = r##"<h1 id="ghost"><a href="#visible"><img src="missing.png" alt="Chart"/></a></h1><h1 id="visible">section</h1><p><a href="#visible">go</a></p>"##;
    for (policy, expected) in [
        (ImagePolicy::Placeholder, "# [\\[Chart\\]](#section)\n\n# section\n\n[go](#section)\n"),
        (ImagePolicy::AltText, "# [Chart](#section)\n\n# section\n\n[go](#section)\n"),
        (ImagePolicy::Omit, "# section\n\n[go](#section)\n"),
    ] {
        assert_eq!(markdown(body, policy), expected);
    }
}
#[test]
fn image_heading_empty_and_unused_targets_do_not_supply_url_labels() {
    for target in ["#missing", "javascript:unused"] {
        let body = format!(
            r##"<h1><a href="{target}"><img src="missing.png" alt="Chart"/></a></h1><h1 id="visible">section</h1><p><a href="#visible">go</a></p>"##
        );
        for (policy, expected) in [
            (ImagePolicy::Placeholder, "# \\[Chart\\]\n\n# section\n\n[go](#section)\n"),
            (ImagePolicy::AltText, "# Chart\n\n# section\n\n[go](#section)\n"),
            (ImagePolicy::Omit, "# section\n\n[go](#section)\n"),
        ] {
            assert_eq!(markdown(&body, policy), expected, "{target:?}");
        }
    }
}
#[test]
fn image_heading_used_text_label_survives_each_policy() {
    let body = r##"<h1 id="target"><a href="https://example.org">Shown<img src="missing.png" alt="Chart"/></a></h1><p><a href="#target">go</a></p>"##;
    for (policy, expected) in [
        (
            ImagePolicy::Placeholder,
            "# [Shown\\[Chart\\]](https://example.org)\n\n[go](#shownchart)\n",
        ),
        (ImagePolicy::AltText, "# [ShownChart](https://example.org)\n\n[go](#shownchart)\n"),
        (ImagePolicy::Omit, "# [Shown](https://example.org)\n\n[go](#shown)\n"),
    ] {
        assert_eq!(markdown(body, policy), expected);
    }
}
