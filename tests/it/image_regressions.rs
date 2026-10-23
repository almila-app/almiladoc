use crate::build::{self, ODF, W, docx_with, epub_bare};
use crate::support::{self, POLICIES, configured, inlines, zip_package};
use almiladoc::model::{Block, Inline};
use almiladoc::{
    Conversion, ConvertError, DiagnosticCode, Format, ImagePolicy, Input, OnLoss, Region,
};

const IMAGE: &str = "<w:p><w:r><w:drawing><a:blip/></w:drawing></w:r></w:p>";
const GOOD: &str = "<w:p><w:r><w:t>First wins.</w:t></w:r></w:p>";

fn docx(body: &str, notes: &[(&str, &str)]) -> Vec<u8> {
    let notes: String = notes
        .iter()
        .map(|(id, body)| format!(r#"<w:footnote w:id="{id}">{body}</w:footnote>"#))
        .collect();
    let footnotes = format!("<w:footnotes {W}>{notes}</w:footnotes>");
    docx_with(body, &[("word/footnotes.xml", footnotes.as_bytes())])
}
fn reference(id: &str) -> String {
    format!(r#"<w:r><w:footnoteReference w:id="{id}"/></w:r>"#)
}
fn paragraph(body: &str) -> String {
    format!("<w:p>{body}</w:p>")
}
fn convert(bytes: &[u8], format: Format, images: ImagePolicy) -> Conversion {
    let converter = configured(|config| {
        config.images = images;
        config.on_loss = OnLoss::Report;
    });
    let parsed = converter.parse(Input::new(bytes, format)).unwrap();
    let result = converter.convert(Input::new(bytes, format)).unwrap();
    assert_eq!(result, parsed.render().unwrap());
    assert_eq!(parsed.diagnostics(), result.diagnostics());
    result
}
fn count(result: &Conversion, code: DiagnosticCode, region: Region) -> u64 {
    support::count(result.diagnostics(), |d| d.code == code && d.region == region)
}

#[test]
fn first_source_definitions_include_blank_and_discard_losing_semantics() {
    let body = paragraph(&format!("<w:r><w:t>Body</w:t></w:r>{}", reference("1")));
    let unplaced = "<w:p><w:r><w:drawing><a:blip/><a:blip/></w:drawing></w:r></w:p>";
    let declined = "<w:p><w:r><mc:AlternateContent><mc:Choice Requires=\"x\"><w:drawing><a:blip/></w:drawing></mc:Choice><mc:Fallback/></mc:AlternateContent></w:r></w:p>";
    for first in [GOOD, "<w:p/>", IMAGE] {
        for loser in [IMAGE, unplaced, declined, "<w:p/>"] {
            let bytes = docx(&body, &[("1", first), ("1", loser)]);
            for policy in POLICIES {
                let result = convert(&bytes, Format::Docx, policy);
                assert_eq!(count(&result, DiagnosticCode::DuplicateNote, Region::Note), 1);
                assert_eq!(count(&result, DiagnosticCode::ImageUnplaced, Region::Note), 0);
                assert_eq!(
                    count(&result, DiagnosticCode::ImageUnavailable, Region::Note),
                    u64::from(first == IMAGE && policy == ImagePolicy::Placeholder)
                );
                if first == GOOD {
                    assert_eq!(result.markdown(), "Body[^1]\n\n[^1]: First wins.\n");
                }
                if first == "<w:p/>" || first == IMAGE && policy == ImagePolicy::Omit {
                    assert_eq!(result.markdown(), "Body\n");
                }
                let strict = configured(|config| config.images = policy);
                assert_eq!(
                    strict
                        .convert(Input::new(&bytes, Format::Docx))
                        .is_err_and(|e| matches!(e, ConvertError::Incomplete { .. })),
                    first == IMAGE && policy == ImagePolicy::Placeholder
                );
                assert_eq!(
                    strict.parse(Input::new(&bytes, Format::Docx)).unwrap().document().assets,
                    []
                );
            }
        }
    }
}

#[test]
fn chosen_note_graph_preserves_nonimage_controls_cycles_and_causal_missing_refs() {
    let body = paragraph(&format!("<w:r><w:t>Body</w:t></w:r>{}", reference("1")));
    let a = paragraph(&reference("2"));
    let b = paragraph(&reference("99"));
    let control = docx(&body, &[("1", &a), ("2", &b)]);
    let with_loser = docx(&body, &[("1", &a), ("2", &b), ("1", IMAGE)]);
    for policy in POLICIES {
        let base = convert(&control, Format::Docx, policy);
        let candidate = convert(&with_loser, Format::Docx, policy);
        assert_eq!(base.markdown(), "Body[^1]\n\n[^1]: [^2]\n");
        assert_eq!(base.markdown(), candidate.markdown());
        assert_eq!(count(&candidate, DiagnosticCode::DuplicateNote, Region::Note), 1);
        assert_eq!(count(&candidate, DiagnosticCode::ImageOmitted, Region::Note), 0);
        assert_eq!(count(&candidate, DiagnosticCode::ImageUnavailable, Region::Note), 0);
        let cycle = docx(&body, &[("1", &a), ("2", &paragraph(&reference("1"))), ("1", GOOD)]);
        assert_eq!(
            convert(&cycle, Format::Docx, policy).markdown(),
            "Body[^1]\n\n[^1]: [^2]\n\n[^2]: [^1]\n"
        );
    }
    let image_missing = format!("{IMAGE}{}", paragraph(&reference("99")));
    let image_owner = docx(&body, &[("1", &image_missing)]);
    let result = convert(&image_owner, Format::Docx, ImagePolicy::Omit);
    assert_eq!(result.markdown(), "Body\n");
    assert_eq!(count(&result, DiagnosticCode::ImageOmitted, Region::Note), 1);
    assert_eq!(count(&result, DiagnosticCode::DanglingReference, Region::Note), 1);
    let blank = convert(&docx(&body, &[("1", "<w:p/>")]), Format::Docx, ImagePolicy::Omit);
    assert_eq!(count(&blank, DiagnosticCode::DanglingReference, Region::Body), 0);
    let absent = convert(&docx(&body, &[]), Format::Docx, ImagePolicy::Omit);
    assert_eq!(count(&absent, DiagnosticCode::DanglingReference, Region::Body), 1);
}

#[test]
fn canonical_note_references_number_depth_first() {
    let body =
        paragraph(&format!("<w:r><w:t>Body</w:t></w:r>{}{}", reference("1"), reference("3")));
    let a = paragraph(&format!("<w:r><w:t>A</w:t></w:r>{}", reference("2")));
    let b = "<w:p><w:r><w:t>B</w:t></w:r></w:p>";
    let c = b.replace('B', "C");
    let bytes = docx(&body, &[("1", &a), ("1", IMAGE), ("2", b), ("3", &c)]);
    for policy in POLICIES {
        assert_eq!(
            convert(&bytes, Format::Docx, policy).markdown(),
            "Body[^1][^3]\n\n[^1]: A[^2]\n\n[^2]: B\n\n[^3]: C\n"
        );
    }
}

fn odf(kind: &str, body: &str) -> Vec<u8> {
    build::odf(&format!("<office:{kind}>{body}</office:{kind}>"))
}
#[test]
fn odf_reserves_nested_note_identity_before_consuming_bodies() {
    let body = r#"<text:p>Body<text:note text:id="n"><text:note-body><text:p>outer<text:note text:id="n"><text:note-body><text:p><draw:frame><draw:image/></draw:frame></text:p></text:note-body></text:note></text:p></text:note-body></text:note><text:note><text:note-body><text:p>anonymous<text:note><text:note-body><text:p>inner</text:p></text:note-body></text:note></text:p></text:note-body></text:note><text:note text:id="odf:a:1"><text:note-body><text:p>named</text:p></text:note-body></text:note></text:p>"#;
    let bytes = odf("text", body);
    for policy in POLICIES {
        let parsed = configured(|config| config.images = policy)
            .parse(Input::new(&bytes, Format::Odt))
            .unwrap();
        assert_eq!(
            parsed.document().notes.iter().map(|note| note.id.as_str()).collect::<Vec<_>>(),
            ["odf:n:n", "odf:a:1", "odf:a:2", "odf:n:odf:a:1"]
        );
        let result = parsed.render().unwrap();
        assert_eq!(count(&result, DiagnosticCode::DuplicateNote, Region::Note), 1);
        assert_eq!(count(&result, DiagnosticCode::ImageUnplaced, Region::Note), 0);
        assert_eq!(parsed.document().assets, []);
        assert!(result.markdown().contains("outer[^1]"));
        assert_eq!(result, convert(&bytes, Format::Odt, policy));
    }
}

#[test]
fn native_frame_representation_precedes_its_unused_preview() {
    let table = "<table:table><table:table-row><table:table-cell><text:p>native</text:p></table:table-cell></table:table-row></table:table>";
    let image = "<draw:image xlink:href=\"missing.png\"/>";
    let body = format!(
        "<draw:page><draw:frame>{table}{image}</draw:frame><draw:frame>{image}</draw:frame></draw:page>"
    );
    let bytes = odf("presentation", &body);
    for policy in POLICIES {
        let result = convert(&bytes, Format::Odp, policy);
        assert_eq!(result.markdown().matches("native").count(), 1);
        assert_eq!(
            count(&result, DiagnosticCode::ImageUnavailable, Region::Body),
            u64::from(policy == ImagePolicy::Placeholder)
        );
        assert_eq!(
            count(&result, DiagnosticCode::ImageOmitted, Region::Body),
            u64::from(policy != ImagePolicy::Placeholder)
        );
        assert_eq!(count(&result, DiagnosticCode::ImagePlaceholder, Region::Body), 0);
    }
}

fn image_count(blocks: &[Block]) -> usize {
    inlines(blocks).into_iter().filter(|inline| matches!(inline, Inline::Image { .. })).count()
}
#[test]
fn code_style_and_cells_preserve_typed_rtf_pictures_and_verified_alt() {
    for style in ["", "\\s1 "] {
        for cell in [false, true] {
            for source in ["\\pngblip 01", "\\wbitmap 01"] {
                let picture = format!(
                    r"{{\pict{{\*\picprop{{\sp{{\sn wzDescription}}{{\sv caf\'e9 \u945?}}}}{{\sp{{\sn wzName}}{{\sv wrong}}}}}}{source}}}"
                );
                let (open, close) = if cell {
                    ("\\trowd\\cellx1000\\intbl ", "\\cell\\row")
                } else {
                    ("", "\\par")
                };
                let bytes = format!(
                    r"{{\rtf1\ansi\ansicpg1252{{\stylesheet{{\s0 Normal;}}{{\s1 Source Code;}}}}\pard\plain {style}{open}before{picture}after{close}}}"
                );
                for policy in POLICIES {
                    let result = convert(bytes.as_bytes(), Format::Rtf, policy);
                    let strict = configured(|config| config.images = policy);
                    let parsed = strict.parse(Input::new(bytes.as_bytes(), Format::Rtf)).unwrap();
                    assert_eq!(image_count(&parsed.document().blocks), 1);
                    assert_eq!(
                        count(&result, DiagnosticCode::ImageUnavailable, Region::Body),
                        u64::from(source.contains("wbitmap") && policy == ImagePolicy::Placeholder)
                    );
                    assert_eq!(
                        strict.convert(Input::new(bytes.as_bytes(), Format::Rtf)).is_err(),
                        source.contains("wbitmap") && policy == ImagePolicy::Placeholder
                    );
                    assert_eq!(result.markdown().contains("café α"), policy != ImagePolicy::Omit);
                    assert!(!result.markdown().contains("wrong"));
                    assert!(
                        result.markdown().contains("before") && result.markdown().contains("after")
                    );
                }
            }
        }
    }
}

#[test]
fn external_image_destination_lookahead_and_link_label_omission_are_contextual() {
    use pulldown_cmark::{Event, Parser, Tag};
    for ending in ["", "&#96;"] {
        let body = format!(
            r#"<p>before&#96;<br/><img src="https://example.org/image{ending}" alt="verified alt"/></p>"#
        );
        for policy in POLICIES {
            let result = convert(&epub_bare(&body), Format::Epub, policy);
            let mut links = Vec::new();
            for event in Parser::new(result.markdown()) {
                match event {
                    Event::Start(Tag::Link { dest_url, .. }) => links.push(dest_url.into_string()),
                    Event::Code(_) => panic!("image URL became a code span: {}", result.markdown()),
                    _ => {}
                }
            }
            let expected = if policy == ImagePolicy::Placeholder {
                vec![format!(
                    "https://example.org/image{}",
                    if ending.is_empty() { "" } else { "`" }
                )]
            } else {
                Vec::new()
            };
            assert_eq!(links, expected);
        }
    }
    let bytes = epub_bare(
        r#"<p><a href="https://example.org/outer"><img src="javascript:unused" alt="label"/></a></p>"#,
    );
    for policy in POLICIES {
        assert_eq!(
            count(&convert(&bytes, Format::Epub, policy), DiagnosticCode::UnsafeLink, Region::Body),
            0
        );
    }
}

#[test]
fn selected_unplaced_images_still_report_and_losing_odf_definitions_do_not() {
    let body = paragraph(&reference("1"));
    for picture in [
        "<w:p><w:r><w:drawing><a:blip/><a:blip/></w:drawing></w:r></w:p>",
        "<w:p><w:r><mc:AlternateContent><mc:Choice Requires=\"x\"><w:drawing><a:blip/></w:drawing></mc:Choice><mc:Fallback/></mc:AlternateContent></w:r></w:p>",
    ] {
        for policy in POLICIES {
            let result = convert(&docx(&body, &[("1", picture)]), Format::Docx, policy);
            assert_eq!(
                count(&result, DiagnosticCode::ImageUnplaced, Region::Note),
                u64::from(policy != ImagePolicy::Omit)
            );
        }
    }
    let picture = "<draw:frame><draw:image xlink:href=\"missing.png\"/></draw:frame>";
    for losing in [false, true] {
        let first = if losing { "<text:p>kept</text:p>" } else { picture };
        let body = format!(
            "<text:p>Body<text:note text:id=\"n\"><text:note-body>{first}</text:note-body></text:note><text:note text:id=\"n\"><text:note-body>{picture}</text:note-body></text:note></text:p>"
        );
        for policy in POLICIES {
            let result = convert(&odf("text", &body), Format::Odt, policy);
            assert_eq!(count(&result, DiagnosticCode::DuplicateNote, Region::Note), 1);
            assert_eq!(
                count(&result, DiagnosticCode::ImageUnplaced, Region::Note),
                u64::from(!losing && policy != ImagePolicy::Omit)
            );
            assert_eq!(
                count(&result, DiagnosticCode::ImageOmitted, Region::Note),
                u64::from(!losing && policy == ImagePolicy::Omit)
            );
        }
    }
}

#[test]
fn unsupported_native_frame_uses_its_readable_image_fallback_once() {
    let bytes = zip_package(&[
        (
            "content.xml",
            format!(
                r#"<office:document-content {ODF}><office:body><office:text><text:p>before<draw:frame><draw:object xlink:href="missing"/><draw:image xlink:href="Pictures/a.png"/></draw:frame>after</text:p></office:text></office:body></office:document-content>"#
            ),
        ),
        ("Pictures/a.png", "png".into()),
    ]);
    for policy in POLICIES {
        let result = convert(&bytes, Format::Odt, policy);
        assert_eq!(count(&result, DiagnosticCode::EmbeddedObject, Region::Body), 1);
        assert_eq!(count(&result, DiagnosticCode::ImageUnavailable, Region::Body), 0);
        assert_eq!(
            result.markdown(),
            match policy {
                ImagePolicy::Placeholder => "before![Image](asset:0)after\n",
                ImagePolicy::AltText => "beforeImageafter\n",
                ImagePolicy::Omit => "beforeafter\n",
                _ => panic!("unexpected policy: {policy:?}"),
            }
        );
        assert_eq!(
            count(&result, DiagnosticCode::ImagePlaceholder, Region::Body),
            u64::from(policy == ImagePolicy::Placeholder)
        );
        assert_eq!(
            count(&result, DiagnosticCode::ImageOmitted, Region::Body),
            u64::from(policy != ImagePolicy::Placeholder)
        );
    }
}
