use super::*;

#[test]
fn image_placeholders_keep_empty_assets_and_unavailable_sources_visible() {
    for (source, expected) in [
        (ImageSource::Asset(AssetId(3)), "![Image](asset:3)\n"),
        (ImageSource::Unavailable, "\\[Image\\]\n"),
    ] {
        let doc = Document {
            blocks: vec![Block::Paragraph(vec![image("", source)])],
            ..Document::default()
        };
        assert_eq!(document_to_markdown(&doc), expected);
    }
}

type Placement = (&'static str, fn(Inline) -> Block);

const PLACEMENTS: [Placement; 3] = [
    ("paragraph", |image| Block::Paragraph(vec![image])),
    ("link", |image| Block::Paragraph(vec![link(vec![image], external("https://example.org"))])),
    ("cell", |image| frame(vec![Block::Paragraph(vec![image])])),
];

struct ImageRead {
    text: String,
    destinations: Vec<String>,
    links: Vec<String>,
}

fn read_images(markdown: &str) -> ImageRead {
    let mut read = ImageRead { text: String::new(), destinations: Vec::new(), links: Vec::new() };
    for event in Parser::new_ext(markdown, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Text(value) => read.text.push_str(&value),
            Event::Start(Tag::Image { dest_url, .. }) => {
                read.destinations.push(dest_url.into_string());
            }
            Event::Start(Tag::Link { dest_url, .. }) => read.links.push(dest_url.into_string()),
            _ => {}
        }
    }
    read
}

fn assert_image_label(
    (alt, label): (&str, &str),
    source: &ImageSource,
    images: ImagePolicy,
    (placement, place): Placement,
) {
    let document =
        Document { blocks: vec![place(image(alt, source.clone()))], ..Document::default() };
    let markdown = rendered(&document, images);
    let read = read_images(&markdown);
    let expected_images =
        usize::from(images == Placeholder && matches!(source, ImageSource::Asset(_)));
    assert_eq!(read.destinations.len(), expected_images, "{alt:?}: {markdown}");
    if expected_images > 0 {
        assert_eq!(read.destinations, ["asset:3"]);
    }
    if images == Omit {
        if placement != "link" {
            assert!(read.text.is_empty(), "{markdown}");
        }
        return;
    }
    let expected = if images == Placeholder
        && matches!(source, ImageSource::Unavailable | ImageSource::Unread)
    {
        format!("[{label}]")
    } else {
        label.to_string()
    };
    assert_eq!(read.text, expected, "{images:?} {placement}: {markdown}");
    if matches!(source, ImageSource::External(_)) && images == Placeholder && placement != "link" {
        assert_eq!(read.links, ["https://example.org/image?a=1&copy;b"]);
    }
}

#[test]
fn image_labels_round_trip_in_paragraphs_links_and_cells_by_policy() {
    for case in [
        ("a[b]*", "a[b]*"),
        ("x|y", "x|y"),
        ("a\\b", "a\\b"),
        ("~~x~~", "~~x~~"),
        ("<script>", "<script>"),
        ("!", "!"),
        ("\\", "\\"),
        ("a\nb", "a b"),
        ("", "Image"),
        ("  picture  ", "picture"),
    ] {
        for source in [
            ImageSource::Asset(AssetId(3)),
            ImageSource::Unavailable,
            ImageSource::Unread,
            ImageSource::External("https://example.org/image?a=1&copy;b".into()),
        ] {
            for images in [Placeholder, AltText, Omit] {
                for placement in PLACEMENTS {
                    assert_image_label(case, &source, images, placement);
                }
            }
        }
    }
}

#[test]
fn image_losses_follow_policy_region_and_the_shown_note_definition() {
    let asset = || image("", ImageSource::Asset(AssetId(0)));
    let content = |region| {
        let shown = |alt: &str, source| Inline::Image {
            origin: ImageOrigin::new(region, None),
            alt: alt.into(),
            source,
        };
        vec![
            shown("", ImageSource::Asset(AssetId(0))),
            shown("", ImageSource::Unavailable),
            shown("", ImageSource::External("https://example.org".into())),
            shown("bad", ImageSource::External("javascript:bad".into())),
        ]
    };
    let mut note_content = content(Region::Note);
    note_content.insert(0, Inline::plain("note: "));
    let document = Document {
        blocks: vec![
            Block::Paragraph(content(Region::Body)),
            frame(vec![Block::Paragraph(vec![link(
                vec![asset()],
                external("https://example.org"),
            )])]),
        ],
        notes: vec![
            note("n", vec![Block::Paragraph(note_content)]),
            note("n", vec![Block::Paragraph(vec![asset(); 5])]),
        ],
        ..Document::default()
    };
    for images in [Placeholder, AltText, Omit] {
        let losses = render_losses(&document, READ, images);
        let count = |code, region| {
            losses
                .iter()
                .filter(|&&(c, r, _, _)| c == code && r == region)
                .map(|&(_, _, _, n)| n)
                .sum::<u64>()
        };
        assert_eq!(count(DiagnosticCode::DuplicateNote, Region::Note), 1);
        if images == Placeholder {
            assert_eq!(count(DiagnosticCode::ImagePlaceholder, Region::Body), 3);
            assert_eq!(count(DiagnosticCode::ImagePlaceholder, Region::Note), 2);
            for region in [Region::Body, Region::Note] {
                assert_eq!(count(DiagnosticCode::ImageOmitted, region), 1);
                assert_eq!(count(DiagnosticCode::UnsafeLink, region), 1);
            }
        } else {
            assert_eq!(count(DiagnosticCode::ImageOmitted, Region::Body), 5);
            assert_eq!(count(DiagnosticCode::ImageOmitted, Region::Note), 4);
            assert_eq!(count(DiagnosticCode::UnsafeLink, Region::Body), 0);
            assert_eq!(count(DiagnosticCode::ImagePlaceholder, Region::Body), 0);
        }
    }
}

#[test]
fn an_unsafe_external_image_is_text_and_omit_has_no_visible_block() {
    let document = Document {
        blocks: vec![Block::Paragraph(vec![image(
            "a[b]",
            ImageSource::External("javascript:bad".into()),
        )])],
        ..Document::default()
    };
    assert_eq!(rendered(&document, Placeholder), "a\\[b]\n");
    assert_eq!(rendered(&document, Omit), "");
}

#[test]
fn omitted_image_notes_and_reference_chains_leave_no_undefined_footnotes() {
    let reference = |id: &str| Inline::NoteRef(id.into());
    let note_of = |id: &str, content| note(id, vec![Block::Paragraph(content)]);
    let image = || Inline::Image {
        origin: ImageOrigin::new(Region::Note, None),
        alt: String::new(),
        source: ImageSource::Asset(AssetId(0)),
    };
    let document = Document {
        blocks: vec![Block::Paragraph(vec![
            Inline::plain("body"),
            reference("a"),
            reference("kept"),
        ])],
        notes: vec![
            note_of("a", vec![reference("b")]),
            note_of("b", vec![image()]),
            note_of("b", vec![image(); 5]),
            note_of("kept", vec![Inline::plain("text")]),
        ],
        ..Document::default()
    };
    assert_eq!(rendered(&document, Omit), "body[^1]\n\n[^1]: text\n");
    assert_eq!(
        render_losses(&document, READ, Omit),
        [
            (DiagnosticCode::DuplicateNote, Region::Note, None, 1),
            (DiagnosticCode::ImageOmitted, Region::Note, None, 1)
        ]
    );
    let cyclic = Document {
        blocks: vec![Block::Paragraph(vec![reference("a")])],
        notes: vec![
            note_of("a", vec![reference("b"), image()]),
            note_of("b", vec![reference("a")]),
        ],
        ..Document::default()
    };
    let markdown = rendered(&cyclic, Omit);
    assert!(markdown.contains("[^1]: [^2]"));
    assert!(markdown.contains("[^2]: [^1]"));
    let missing = Document {
        blocks: vec![Block::Paragraph(vec![reference("absent")])],
        ..Document::default()
    };
    assert_eq!(
        render_losses(&missing, READ, Omit),
        [(DiagnosticCode::DanglingReference, Region::Body, None, 1)]
    );
}

#[test]
fn an_omitted_note_reports_its_images_and_no_other_loss() {
    let omitted = Inline::Image {
        origin: ImageOrigin::new(Region::Note, None),
        alt: String::new(),
        source: ImageSource::Asset(AssetId(0)),
    };
    let missing = link(Vec::new(), LinkTarget::anchor("nowhere".into()));
    let document = Document {
        blocks: vec![Block::Paragraph(vec![Inline::plain("body"), Inline::NoteRef("n".into())])],
        notes: vec![note("n", vec![Block::Paragraph(vec![omitted, missing])])],
        ..Document::default()
    };
    assert_eq!(rendered(&document, Omit), "body\n");
    assert_eq!(
        render_losses(&document, READ, Omit),
        [(DiagnosticCode::ImageOmitted, Region::Note, None, 1)]
    );
}

#[test]
fn omitted_images_do_not_split_equally_styled_runs() {
    for style in [BOLD, ITALIC, STRIKE, CODE] {
        let document = Document {
            blocks: vec![Block::Paragraph(vec![
                styled("a", style),
                image("picture", ImageSource::Unavailable),
                styled("b", style),
            ])],
            ..Document::default()
        };
        assert_eq!(
            rendered(&document, Omit),
            doc(vec![Block::Paragraph(vec![styled("ab", style)])])
        );
    }
}

#[test]
fn image_destinations_cannot_close_preceding_code() {
    let url = "https://example.org/a`b";
    let markdown = doc(vec![Block::Paragraph(vec![
        Inline::plain("`"),
        image("label", ImageSource::External(url.into())),
    ])]);
    assert_eq!(parsed_destinations(&markdown), [url]);
    assert_eq!(parsed_text(&markdown), "`label");
}

#[test]
fn image_losses_follow_source_origins_across_rendered_containers() {
    let speaker = ImageOrigin::new(Region::Note, Some(unit(UnitKind::Slide, 1)));
    let body = ImageOrigin::new(Region::Body, None);
    let image = |source, origin| Inline::Image { alt: "image".into(), source, origin };
    let document = Document {
        blocks: vec![Block::BlockQuote(vec![Block::Paragraph(vec![
            image(ImageSource::Asset(AssetId(0)), speaker),
            image(ImageSource::Unavailable, speaker),
            image(ImageSource::Unread, speaker),
            image(ImageSource::External("javascript:bad".into()), speaker),
        ])])],
        notes: vec![note("n", vec![Block::Paragraph(vec![image(ImageSource::Unread, body)])])],
        ..Document::default()
    };
    for policy in [Placeholder, AltText, Omit] {
        let mut actual = render_losses(&document, READ, policy);
        let mut expected = if policy == Placeholder {
            vec![
                (DiagnosticCode::ImagePlaceholder, speaker.region, speaker.unit, 2),
                (DiagnosticCode::ImageUnavailable, speaker.region, speaker.unit, 1),
                (DiagnosticCode::UnsafeLink, speaker.region, speaker.unit, 1),
                (DiagnosticCode::ImageUnavailable, body.region, body.unit, 1),
            ]
        } else {
            vec![
                (DiagnosticCode::ImageOmitted, speaker.region, speaker.unit, 4),
                (DiagnosticCode::ImageOmitted, body.region, body.unit, 1),
            ]
        };
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected, "{policy:?}");
    }
}
