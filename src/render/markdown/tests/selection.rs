use super::*;

fn note_image(alt: &str) -> Inline {
    Inline::Image {
        origin: ImageOrigin::new(Region::Note, None),
        alt: alt.into(),
        source: ImageSource::Unread,
    }
}

fn go(id: &str) -> Inline {
    link(vec![Inline::plain("go")], LinkTarget::anchor(id.into()))
}

#[test]
fn heading_slugs_follow_the_emitted_image_labels_and_visibility() {
    for (alt, label, slug) in [("  A\n B  ", "A  B", "a--b"), ("", "Image", "image")] {
        let document = Document {
            blocks: vec![
                anchored(1, "picture", vec![note_image(alt)]),
                anchored(1, "text", vec![Inline::plain(label)]),
                Block::Paragraph(vec![go("picture"), Inline::plain(" "), go("text")]),
            ],
            ..Document::default()
        };
        for policy in [Placeholder, AltText, Omit] {
            let output = rendered(&document, policy);
            if policy == Omit {
                assert_eq!(output, format!("# {label}\n\ngo [go](#{slug})\n"));
            } else {
                assert!(output.ends_with(&format!("[go](#{slug}) [go](#{slug}-1)\n")), "{output}");
                assert_eq!(output.lines().filter(|line| line.starts_with("# ")).count(), 2);
            }
        }
    }
}

#[test]
fn losing_and_image_pruned_notes_cannot_claim_anchors_or_heading_slugs() {
    for first in [vec![], vec![Block::Paragraph(vec![Inline::plain("first")])]] {
        let document = Document {
            blocks: vec![
                Block::Paragraph(vec![go("ghost")]),
                anchored(1, "real", vec![Inline::plain("Same")]),
                Block::Paragraph(vec![go("real")]),
            ],
            notes: vec![
                note("n", first),
                note("n", vec![anchored(1, "ghost", vec![Inline::plain("Same")])]),
            ],
            ..Document::default()
        };
        for policy in [Placeholder, AltText, Omit] {
            let output = rendered(&document, policy);
            assert!(output.starts_with("go\n\n# Same\n\n[go](#same)\n"), "{output}");
            assert!(!output.contains("#same-1"));
            assert_eq!(
                render_losses(&document, READ, policy)
                    .iter()
                    .filter(|loss| loss.0 == DiagnosticCode::DanglingReference)
                    .map(|loss| loss.3)
                    .sum::<u64>(),
                1
            );
        }
    }
    let document = Document {
        blocks: vec![Block::Paragraph(vec![
            Inline::plain("body"),
            Inline::NoteRef("n".into()),
            go("ghost"),
        ])],
        notes: vec![note(
            "n",
            vec![Block::Paragraph(vec![
                Inline::Anchor("ghost".into()),
                note_image(""),
                Inline::NoteRef("missing".into()),
            ])],
        )],
        ..Document::default()
    };
    assert_eq!(rendered(&document, Omit), "bodygo\n");
    assert_eq!(
        render_losses(&document, READ, Omit),
        [
            (DiagnosticCode::DanglingReference, Region::Body, None, 1),
            (DiagnosticCode::DanglingReference, Region::Note, None, 1),
            (DiagnosticCode::ImageOmitted, Region::Note, None, 1),
        ]
    );
}

#[test]
fn heading_internal_blank_labels_keep_spacing_independent_of_target_resolution() {
    for target in [
        LinkTarget::anchor("real".into()),
        LinkTarget::anchor("missing".into()),
        LinkTarget::anchor(String::new()),
    ] {
        let document = Document {
            blocks: vec![
                Block::heading(1, vec![note_image(""), Inline::NoteRef("missing".into())]),
                anchored(
                    1,
                    "title",
                    vec![
                        Inline::plain("a"),
                        link(vec![Inline::plain(" "), note_image(""), Inline::plain(" ")], target),
                        Inline::plain("b"),
                    ],
                ),
                anchored(1, "real", vec![Inline::plain("section")]),
                Block::Paragraph(vec![go("title"), Inline::plain(" "), go("real")]),
            ],
            ..Document::default()
        };
        assert_eq!(
            rendered(&document, Omit),
            "# a  b\n\n# section\n\n[go](#a--b) [go](#section)\n"
        );
    }
}

#[test]
fn heading_empty_targets_keep_their_image_labels_without_a_link_wrapper() {
    for target in
        [LinkTarget::anchor(String::new()), external(""), LinkTarget::Relative(String::new())]
    {
        let document = Document {
            blocks: vec![
                Block::heading(1, vec![link(vec![note_image("Chart")], target)]),
                anchored(1, "real", vec![Inline::plain("section")]),
                Block::Paragraph(vec![go("real")]),
            ],
            ..Document::default()
        };
        for (policy, expected) in [
            (Placeholder, "# \\[Chart\\]\n\n# section\n\n[go](#section)\n"),
            (AltText, "# Chart\n\n# section\n\n[go](#section)\n"),
            (Omit, "# section\n\n[go](#section)\n"),
        ] {
            assert_eq!(rendered(&document, policy), expected);
        }
    }
}

#[test]
fn nested_degraded_heading_labels_keep_context_and_outer_url_fallback() {
    for depth in 1..=2 {
        let mut content = vec![image("Chart", ImageSource::External("javascript:unused".into()))];
        for _ in 0..depth {
            content = vec![link(content, LinkTarget::anchor("missing".into()))];
        }
        let document = Document {
            blocks: vec![
                anchored(1, "title", vec![link(content, external("https://outer.test"))]),
                Block::Paragraph(vec![go("title")]),
            ],
            ..Document::default()
        };
        for (policy, expected) in [
            (Placeholder, "# [Chart](https://outer.test)\n\n[go](#chart)\n"),
            (AltText, "# [Chart](https://outer.test)\n\n[go](#chart)\n"),
            (Omit, "# [https://outer.test](https://outer.test)\n\n[go](#httpsoutertest)\n"),
        ] {
            assert_eq!(rendered(&document, policy), expected);
            assert!(
                !render_losses(&document, READ, policy)
                    .iter()
                    .any(|loss| loss.0 == DiagnosticCode::UnsafeLink)
            );
        }
    }
}
