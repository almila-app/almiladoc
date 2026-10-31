use crate::build::{docx_with, pptx, relationships, slide_part};
use crate::support::{POLICIES, configured, count, inlines, reported};
use almiladoc::model::{Block, ImageSource, Inline};
use almiladoc::{DiagnosticCode, Format, ImagePolicy, Input, OnLoss, Region};

fn picture(id: &str, hidden: &str) -> String {
    format!(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="2" name="pic" hidden="{hidden}"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="{id}"/></p:blipFill></p:pic>"#
    )
}
fn image_sources(blocks: &[Block]) -> Vec<ImageSource> {
    let image = |inline: &Inline| match inline {
        Inline::Image { source, .. } => Some(source.clone()),
        _ => None,
    };
    inlines(blocks).into_iter().filter_map(image).collect()
}

fn speaker_note_pptx() -> Vec<u8> {
    let group = format!(
        "<p:grpSp><p:nvGrpSpPr><p:cNvPr id=\"3\" name=\"group\"/></p:nvGrpSpPr>{}</p:grpSp>",
        picture("missing", "false")
    );
    let hidden = format!(
        "<p:grpSp><p:nvGrpSpPr><p:cNvPr id=\"4\" name=\"hidden\" hidden=\"true\"/></p:nvGrpSpPr>{}</p:grpSp>",
        picture("hidden", "false")
    );
    let footer = r#"<p:sp><p:nvSpPr><p:cNvPr id="5" name="footer"/><p:nvPr><p:ph type="ftr"/></p:nvPr></p:nvSpPr><p:spPr><a:blipFill><a:blip r:embed="footer"/></a:blipFill></p:spPr><p:txBody><a:p><a:r><a:t>Footer</a:t></a:r></a:p></p:txBody></p:sp>"#;
    let tree = format!(
        "{}{}{}{}{}",
        picture("asset", "false"),
        group,
        hidden,
        picture("hidden", "1"),
        footer
    );
    let slide_rels = relationships(&[("notes", "notesSlide", "../notesSlides/notesSlide1.xml")]);
    let notes = slide_part("notes", &tree);
    let notes_rels = relationships(&[("asset", "image", "../media/image.png")]);
    pptx(
        "",
        &[
            ("ppt/slides/_rels/slide1.xml.rels", slide_rels.as_bytes()),
            ("ppt/notesSlides/notesSlide1.xml", notes.as_bytes()),
            ("ppt/notesSlides/_rels/notesSlide1.xml.rels", notes_rels.as_bytes()),
            ("ppt/media/image.png", b"png"),
        ],
    )
}

#[test]
fn speaker_note_pictures_use_shape_selection_source_order_and_note_origins() {
    let bytes = speaker_note_pptx();
    for policy in POLICIES {
        let converter = configured(|config| {
            config.images = policy;
            config.on_loss = OnLoss::Report;
        });
        let parsed = converter.parse(Input::new(&bytes, Format::Pptx)).unwrap();
        let sources = image_sources(&parsed.document().blocks);
        assert!(matches!(sources.as_slice(), [ImageSource::Asset(_), ImageSource::Unread]));
        let result = converter.convert(Input::new(&bytes, Format::Pptx)).unwrap();
        assert_eq!(result, parsed.render().unwrap());
        let losses: Vec<_> = reported(result.diagnostics())
            .into_iter()
            .filter(|&(code, ..)| {
                matches!(
                    code,
                    DiagnosticCode::ImageUnavailable
                        | DiagnosticCode::ImagePlaceholder
                        | DiagnosticCode::ImageOmitted
                )
            })
            .collect();
        if policy == ImagePolicy::Placeholder {
            assert_eq!(
                losses,
                [
                    (DiagnosticCode::ImageUnavailable, Region::Note, Some(1), 1),
                    (DiagnosticCode::ImagePlaceholder, Region::Note, Some(1), 1)
                ]
            );
            let text = result.markdown();
            assert!(text.find("asset:0").unwrap() < text.find("\\[Image\\]").unwrap());
        } else {
            assert_eq!(losses, [(DiagnosticCode::ImageOmitted, Region::Note, Some(1), 2)]);
        }
        assert!(!result.markdown().contains("Footer"));
    }
}

#[test]
fn failed_relationships_keep_actual_placed_images_unread() {
    let docx = docx_with(
        r#"<w:p><w:r><w:drawing><a:blip r:embed="missing"/></w:drawing></w:r></w:p>"#,
        &[("word/_rels/document.xml.rels", b"</Relationships>")],
    );
    let pptx = pptx(
        &picture("missing", "false"),
        &[("ppt/slides/_rels/slide1.xml.rels", b"</Relationships>")],
    );
    for (format, bytes) in [(Format::Docx, docx), (Format::Pptx, pptx)] {
        for policy in POLICIES {
            let converter = configured(|config| {
                config.images = policy;
                config.on_loss = OnLoss::Report;
            });
            let parsed = converter.parse(Input::new(&bytes, format)).unwrap();
            assert_eq!(image_sources(&parsed.document().blocks), [ImageSource::Unread]);
            let result = converter.convert(Input::new(&bytes, format)).unwrap();
            assert_eq!(result, parsed.render().unwrap());
            let found = |code| count(result.diagnostics(), |d| d.code == code);
            assert_eq!(found(DiagnosticCode::PartUnreadable), 1);
            assert_eq!(
                found(DiagnosticCode::ImageUnavailable),
                u64::from(policy == ImagePolicy::Placeholder)
            );
        }
    }
}

const OLE_MEDIA: &str = "application/vnd.ms-ole-object";

const VML_SHAPE_ELEMENTS: [&str; 8] =
    ["rect", "roundrect", "oval", "line", "polyline", "arc", "curve", "image"];

fn vml_shape(element: &str, attributes: &str, rel: Option<&str>) -> String {
    let picture = rel.map_or_else(String::new, |id| format!(r#"<v:imagedata r:id="{id}"/>"#));
    format!("<v:{element} {attributes}>{picture}</v:{element}>")
}

fn vml_picture(element: &str, shape: &str, rel: &str) -> String {
    vml_shape(element, &format!(r#"id="{shape}""#), Some(rel))
}

fn docx_object(preview: Option<&str>, descr: &str, rpr: &str) -> Vec<u8> {
    let shape = preview
        .map_or_else(|| r#"<v:shape id="s1"/>"#.to_string(), |id| vml_picture("shape", "s1", id));
    docx_drawn(&shape, r#"ShapeID="s1""#, descr, rpr)
}

fn docx_drawn(shapes: &str, shape_id: &str, descr: &str, rpr: &str) -> Vec<u8> {
    docx_run(&format!(
        r#"{rpr}<w:object xmlns:o="urn:schemas-microsoft-com:office:office"><wp:docPr id="1" name="Object 1" descr="{descr}"/>{shapes}<o:OLEObject ProgID="Excel.Sheet.12" {shape_id} r:id="ole"/></w:object>"#
    ))
}

fn docx_run(content: &str) -> Vec<u8> {
    let body = format!("<w:p><w:r>{content}</w:r></w:p>");
    let rels = relationships(&[
        ("ole", "oleObject", "embeddings/one.bin"),
        ("png", "image", "media/one.png"),
        ("two", "image", "media/two.png"),
        ("gone", "image", "media/gone.png"),
    ])
    .replace(
        "</Relationships>",
        r#"<Relationship Id="linked" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/oleObject" Target="file:///C:/sheet.xlsx" TargetMode="External"/></Relationships>"#,
    );
    docx_with(
        &body,
        &[
            ("word/_rels/document.xml.rels", rels.as_bytes()),
            ("word/embeddings/one.bin", b"ole"),
            ("word/media/one.png", b"png"),
            ("word/media/two.png", b"two"),
        ],
    )
}

fn drawn(rel: &str) -> String {
    format!(r#"<w:drawing><a:blip r:embed="{rel}"/></w:drawing>"#)
}

fn in_instruction(content: &str) -> Vec<u8> {
    let field = |kind| format!(r#"<w:fldChar w:fldCharType="{kind}"/>"#);
    docx_run(&format!(
        r#"{}<w:instrText xml:space="preserve">REF here</w:instrText>{content}{}<w:t>result</w:t>{}"#,
        field("begin"),
        field("separate"),
        field("end")
    ))
}

fn ole_picture(rel: &str) -> String {
    format!(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="0" name="" descr="inner"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="{rel}"/></p:blipFill><p:spPr/></p:pic>"#
    )
}

fn pptx_object(preview: Option<&str>, descr: &str, hidden: &str) -> Vec<u8> {
    pptx_drawn(&preview.map_or_else(String::new, ole_picture), descr, hidden)
}

fn pptx_drawn(pic: &str, descr: &str, hidden: &str) -> Vec<u8> {
    let object = |inner: &str| {
        format!(
            r#"<p:oleObj name="Worksheet" progId="Excel.Sheet.12" r:id="ole">{inner}</p:oleObj>"#
        )
    };
    let tree = format!(
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="5" name="Sheet" descr="{descr}" hidden="{hidden}"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/presentationml/2006/ole"><mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml"><mc:Choice Requires="v">{}</mc:Choice><mc:Fallback>{}</mc:Fallback></mc:AlternateContent></a:graphicData></a:graphic></p:graphicFrame>"#,
        object(""),
        object(&format!("<p:embed/>{pic}")),
    );
    let rels = relationships(&[
        ("ole", "oleObject", "../embeddings/one.bin"),
        ("png", "image", "../media/one.png"),
        ("two", "image", "../media/two.png"),
        ("gone", "image", "../media/gone.png"),
    ]);
    pptx(
        &tree,
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/embeddings/one.bin", b"ole"),
            ("ppt/media/one.png", b"png"),
            ("ppt/media/two.png", b"two"),
        ],
    )
}

fn object_outcome(
    format: Format,
    bytes: &[u8],
    policy: ImagePolicy,
) -> (String, Vec<(DiagnosticCode, u64)>, usize) {
    let converter = configured(|config| {
        config.images = policy;
        config.on_loss = OnLoss::Report;
    });
    let parsed = converter.parse(Input::new(bytes, format)).unwrap();
    let payloads = parsed.document().assets.iter().filter(|a| a.media_type == OLE_MEDIA).count();
    let result = parsed.render().unwrap();
    let mut codes: Vec<_> = result.diagnostics().iter().map(|d| (d.code, d.count)).collect();
    codes.sort();
    (result.markdown().trim_end().to_string(), codes, payloads)
}

#[test]
fn an_object_shows_its_preview_and_keeps_its_payload_unreferenced() {
    use DiagnosticCode::{EmbeddedObject, ImageOmitted, ImagePlaceholder, ImageUnavailable};
    let paired = Some(("Sales table", "![Sales table](asset:0)", ImagePlaceholder));
    let missing = Some(("Sales table", "\\[Sales table\\]", ImageUnavailable));
    let stand_in = Some(("Sales table", "\\[Sales table\\]", ImagePlaceholder));
    let undescribed = Some(("Image", "![Image](asset:0)", ImagePlaceholder));
    let cases = [
        ("a paired preview", Format::Docx, docx_object(Some("png"), "Sales table", ""), paired),
        ("a preview without alt", Format::Docx, docx_object(Some("png"), "", ""), undescribed),
        ("a missing preview", Format::Docx, docx_object(Some("gone"), "Sales table", ""), missing),
        ("a described object", Format::Docx, docx_object(None, "Sales table", ""), stand_in),
        ("a bare object", Format::Docx, docx_object(None, " ", ""), None),
        ("a fallback preview", Format::Pptx, pptx_object(Some("png"), "Sales table", "0"), paired),
        (
            "a slide preview without alt",
            Format::Pptx,
            pptx_object(Some("png"), "", "0"),
            undescribed,
        ),
        (
            "a missing slide preview",
            Format::Pptx,
            pptx_object(Some("gone"), "Sales table", "0"),
            missing,
        ),
        ("a described slide object", Format::Pptx, pptx_object(None, "Sales table", "0"), stand_in),
        ("a bare slide object", Format::Pptx, pptx_object(None, "", "0"), None),
    ];
    for (case, format, bytes, placed) in cases {
        for policy in POLICIES {
            let (markdown, image) = match (placed, policy) {
                (None, _) => (String::new(), None),
                (Some((_, shown, code)), ImagePolicy::Placeholder) => (shown.into(), Some(code)),
                (Some((label, ..)), ImagePolicy::AltText) => (label.into(), Some(ImageOmitted)),
                (Some(_), _) => (String::new(), Some(ImageOmitted)),
            };
            let mut codes = vec![(EmbeddedObject, 1)];
            codes.extend(image.map(|code| (code, 1)));
            codes.sort();
            let expected = (markdown, codes, 1);
            assert_eq!(object_outcome(format, &bytes, policy), expected, "{case} {policy:?}");
        }
    }
}

#[test]
fn a_hidden_object_keeps_and_shows_nothing() {
    use DiagnosticCode::{EmbeddedObject, HiddenOmitted, ImagePlaceholder};
    let hidden = [
        (Format::Docx, docx_object(Some("png"), "Sales table", "<w:rPr><w:vanish/></w:rPr>")),
        (Format::Pptx, pptx_object(Some("png"), "Sales table", "1")),
    ];
    for (format, bytes) in hidden {
        for policy in POLICIES {
            let expected = (String::new(), vec![(HiddenOmitted, 1)], 0);
            assert_eq!(object_outcome(format, &bytes, policy), expected, "{format:?} {policy:?}");
        }
    }
    let visible = [
        (
            Format::Docx,
            docx_object(Some("png"), "Sales table", "<w:rPr><w:vanish w:val=\"0\"/></w:rPr>"),
        ),
        (Format::Pptx, pptx_object(Some("png"), "Sales table", "0")),
    ];
    for (format, bytes) in visible {
        let shown = (
            "![Sales table](asset:0)".to_string(),
            vec![(ImagePlaceholder, 1), (EmbeddedObject, 1)],
            1,
        );
        assert_eq!(object_outcome(format, &bytes, ImagePolicy::Placeholder), shown, "{format:?}");
    }
}

fn shown_bytes(format: Format, bytes: &[u8]) -> Vec<Vec<u8>> {
    let parsed = configured(|config| config.on_loss = OnLoss::Report)
        .parse(Input::new(bytes, format))
        .unwrap();
    let document = parsed.document();
    let asset = |source: ImageSource| match source {
        ImageSource::Asset(id) => Some(document.assets[id.0].bytes.clone()),
        _ => None,
    };
    image_sources(&document.blocks).into_iter().filter_map(asset).collect()
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of previews by shape id")]
fn an_object_shows_only_the_picture_its_shape_id_names() {
    use DiagnosticCode::{EmbeddedObject, ImagePlaceholder, ImageUnplaced};
    let decoy = vml_picture("shape", "s2", "two");
    let named = format!("{decoy}{}", vml_picture("shape", "s1", "png"));
    let in_rect = vml_picture("rect", "s1", "png");
    let after_oval = format!("{}{in_rect}", vml_picture("oval", "s2", "two"));
    let unnamed = vml_picture("shape", "s1", "two");
    let pair = r#"<v:shape id="s1"><v:imagedata r:id="png"/><v:imagedata r:id="two"/></v:shape>"#;
    let fill_then_picture =
        r#"<v:shape id="s1"><v:fill r:id="two" type="frame"/><v:imagedata r:id="png"/></v:shape>"#;
    let picture_then_fill =
        r#"<v:shape id="s1"><v:imagedata r:id="png"/><v:fill r:id="two" type="frame"/></v:shape>"#;
    let fill_alone = r#"<v:rect id="s1"><v:fill r:id="png" type="frame"/></v:rect>"#;
    let off_fill_then_picture = r#"<v:shape id="s1" filled="f"><v:fill r:id="two" type="frame"/><v:imagedata r:id="png"/></v:shape>"#;
    let off_fill_alone =
        r#"<v:rect id="s1" filled="false"><v:fill r:id="png" type="frame"/></v:rect>"#;
    let off_fill_decoy = format!(
        r#"<v:rect id="s2" filled="f"><v:fill r:id="two" type="frame"/></v:rect>{}"#,
        vml_picture("shape", "s1", "png")
    );
    let fill_decoy = format!(
        r#"<v:rect id="s2"><v:fill r:id="two" type="frame"/></v:rect>{}"#,
        vml_picture("shape", "s1", "png")
    );
    let fill_then_drawing =
        format!(r#"<v:group><v:fill r:id="two" type="frame"/></v:group>{}"#, drawn("png"));
    let idless_before_drawing =
        format!("{}{}", vml_shape("shape", r#"alt="Idless alt""#, Some("two")), drawn("png"));
    let selected = |declined: &str| {
        format!(
            r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml"><mc:Choice Requires="v">{}</mc:Choice><mc:Fallback>{}</mc:Fallback></mc:AlternateContent>"#,
            ole_picture(declined),
            ole_picture("png"),
        )
    };
    let docx = |shapes: &str, shape_id: &str| docx_drawn(shapes, shape_id, "Sales table", "");
    let png: Option<&[u8]> = Some(b"png");
    let cases = [
        ("a decoy before the preview", Format::Docx, docx(&named, r#"ShapeID="s1""#), png, 1),
        ("a rect preview", Format::Docx, docx(&in_rect, r#"ShapeID="s1""#), png, 0),
        (
            "a decoy before a rect preview",
            Format::Docx,
            docx(&after_oval, r#"ShapeID="s1""#),
            png,
            1,
        ),
        ("a decoy and no preview", Format::Docx, docx(&decoy, r#"ShapeID="s1""#), None, 1),
        ("an id of another case", Format::Docx, docx(&decoy, r#"ShapeID="S2""#), None, 1),
        ("a foreign-namespace id", Format::Docx, docx(&decoy, r#"w:ShapeID="s2""#), None, 1),
        ("no shape id", Format::Docx, docx(&unnamed, ""), None, 1),
        ("no shape id beside a drawing", Format::Docx, docx(&drawn("png"), ""), png, 0),
        (
            "no shape id beside an id-less shape before a drawing",
            Format::Docx,
            docx(&idless_before_drawing, ""),
            png,
            1,
        ),
        (
            "a shape id naming none beside a drawing",
            Format::Docx,
            docx(&drawn("png"), r#"ShapeID="s9""#),
            png,
            0,
        ),
        ("two pictures in the shape", Format::Docx, docx(pair, r#"ShapeID="s1""#), png, 1),
        (
            "a fill before the picture of its shape",
            Format::Docx,
            docx(fill_then_picture, r#"ShapeID="s1""#),
            png,
            1,
        ),
        (
            "a fill after the picture of its shape",
            Format::Docx,
            docx(picture_then_fill, r#"ShapeID="s1""#),
            png,
            1,
        ),
        ("a fill alone in its shape", Format::Docx, docx(fill_alone, r#"ShapeID="s1""#), png, 0),
        (
            "a fill decoy before the preview",
            Format::Docx,
            docx(&fill_decoy, r#"ShapeID="s1""#),
            png,
            1,
        ),
        (
            "a fill its shape switched off before the picture of its shape",
            Format::Docx,
            docx(off_fill_then_picture, r#"ShapeID="s1""#),
            png,
            0,
        ),
        (
            "a fill its shape switched off alone",
            Format::Docx,
            docx(off_fill_alone, r#"ShapeID="s1""#),
            None,
            0,
        ),
        (
            "a fill its shape switched off before the preview",
            Format::Docx,
            docx(&off_fill_decoy, r#"ShapeID="s1""#),
            png,
            0,
        ),
        (
            "a fill before a drawing no shape holds",
            Format::Docx,
            docx(&fill_then_drawing, r#"ShapeID="s9""#),
            png,
            1,
        ),
        (
            "a branch in the object",
            Format::Pptx,
            pptx_drawn(&selected("two"), "Sales table", "0"),
            png,
            0,
        ),
    ];
    for (case, format, bytes, preview, unplaced) in cases {
        let mut codes = vec![(EmbeddedObject, 1), (ImagePlaceholder, 1), (ImageUnplaced, unplaced)];
        codes.retain(|&(_, count)| count > 0);
        codes.sort();
        let markdown =
            if preview.is_some() { "![Sales table](asset:0)" } else { "\\[Sales table\\]" };
        let expected = (markdown.to_string(), codes, 1);
        assert_eq!(object_outcome(format, &bytes, ImagePolicy::Placeholder), expected, "{case}");
        let shown: Vec<Vec<u8>> = preview.into_iter().map(<[u8]>::to_vec).collect();
        assert_eq!(shown_bytes(format, &bytes), shown, "{case}");
    }
}

fn assert_labelled(cases: &[(&str, Vec<u8>, &str, &str, u64)], payloads: u64) {
    use DiagnosticCode::{EmbeddedObject, ImageOmitted, ImagePlaceholder, ImageUnplaced};
    for (case, bytes, label, placeholder, unplaced) in cases {
        for (policy, markdown, image) in [
            (ImagePolicy::Placeholder, placeholder, ImagePlaceholder),
            (ImagePolicy::AltText, label, ImageOmitted),
        ] {
            let mut codes =
                vec![(EmbeddedObject, payloads), (image, 1), (ImageUnplaced, *unplaced)];
            codes.retain(|&(_, count)| count > 0);
            codes.sort();
            let expected = ((*markdown).to_string(), codes, usize::try_from(payloads).unwrap());
            assert_eq!(object_outcome(Format::Docx, bytes, policy), expected, "{case} {policy:?}");
        }
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of VML picture labels by shape")]
fn a_vml_picture_is_labelled_by_the_alt_of_its_own_shape() {
    let pict = |shapes: &[String], docpr: &str| {
        docx_run(&format!("<w:pict>{docpr}{}</w:pict>", shapes.concat()))
    };
    let shape = |id: &str, alt: &str, rel: &str| {
        vml_shape("shape", &format!(r#"id="{id}" alt="{alt}""#), Some(rel))
    };
    let bare = |id: &str, rel: &str| vml_picture("shape", id, rel);
    let held =
        |element: &str| pict(&[vml_shape(element, r#"id="a" alt="Element alt""#, Some("png"))], "");
    let described = r#"<wp:docPr id="1" name="Picture 1" descr="Doc descr"/>"#;
    let grouped = |attributes: &str, shapes: &[String]| {
        docx_run(&format!("<w:pict><v:group {attributes}>{}</v:group></w:pict>", shapes.concat()))
    };
    let cases = [
        (
            "a shape alt",
            pict(&[shape("a", "A pict alt", "png")], ""),
            "A pict alt",
            "![A pict alt](asset:0)",
            0,
        ),
        (
            "an alt cleaned as text",
            pict(&[shape("a", "A&#x200B; pict&#10;alt", "png")], ""),
            "A pict alt",
            "![A pict alt](asset:0)",
            0,
        ),
        ("a shape without alt", pict(&[bare("a", "png")], ""), "Image", "![Image](asset:0)", 0),
        ("a blank alt", pict(&[shape("a", " &#9; ", "png")], ""), "Image", "![Image](asset:0)", 0),
        (
            "a shape alt over the drawing description",
            pict(&[shape("a", "A pict alt", "png")], described),
            "A pict alt",
            "![A pict alt](asset:0)",
            0,
        ),
        (
            "a blank alt under the drawing description",
            pict(&[shape("a", " ", "png")], described),
            "Doc descr",
            "![Doc descr](asset:0)",
            0,
        ),
        (
            "a shape without alt under the drawing description",
            pict(&[bare("a", "png")], described),
            "Doc descr",
            "![Doc descr](asset:0)",
            0,
        ),
        (
            "a group alt",
            grouped(r#"alt="Group alt""#, &[bare("a", "png")]),
            "Image",
            "![Image](asset:0)",
            0,
        ),
        (
            "a group alt over a picture of no shape",
            grouped(r#"alt="Group alt""#, &[r#"<v:imagedata r:id="png"/>"#.to_string()]),
            "Image",
            "![Image](asset:0)",
            0,
        ),
        (
            "the placed shape of a group",
            grouped("", &[shape("a", "First alt", "png"), shape("b", "Second alt", "two")]),
            "First alt",
            "![First alt](asset:0)",
            1,
        ),
        (
            "a bare placed shape before one with alt",
            grouped("", &[bare("a", "png"), shape("b", "Second alt", "two")]),
            "Image",
            "![Image](asset:0)",
            1,
        ),
        (
            "a shape alt before the shape with the picture",
            grouped(
                "",
                &[
                    vml_shape("shape", r#"id="a" alt="Decoy alt""#, None),
                    shape("b", "B alt", "png"),
                ],
            ),
            "B alt",
            "![B alt](asset:0)",
            0,
        ),
        (
            "a foreign-namespace rect alt",
            pict(
                &[r#"<x:rect xmlns:x="urn:other" id="a" alt="Foreign alt"><v:imagedata r:id="png"/></x:rect>"#.to_string()],
                "",
            ),
            "Image",
            "![Image](asset:0)",
            0,
        ),
        (
            "a shape type alt",
            pict(&[vml_shape("shapetype", r#"id="a" alt="Type alt""#, Some("png"))], ""),
            "Image",
            "![Image](asset:0)",
            0,
        ),
    ];
    let elements = VML_SHAPE_ELEMENTS
        .map(|element| (element, held(element), "Element alt", "![Element alt](asset:0)", 0));
    assert_labelled(&cases.into_iter().chain(elements).collect::<Vec<_>>(), 0);
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of what a VML picture fill shows")]
fn a_vml_picture_fill_is_the_picture_of_its_shape_after_every_other_picture() {
    let pict = |content: &str, docpr: &str| docx_run(&format!("<w:pict>{docpr}{content}</w:pict>"));
    let rect = |attributes: &str, inner: &str| format!("<v:rect {attributes}>{inner}</v:rect>");
    let frame = |rel: &str| format!(r#"<v:fill r:id="{rel}" type="frame"/>"#);
    let picture = r#"<v:imagedata r:id="png"/>"#;
    let described = r#"<wp:docPr id="1" name="Picture 1" descr="Doc descr"/>"#;
    let picture_alt = vml_shape("shape", r#"id="b" alt="Picture alt""#, Some("png"));
    let drawing = r#"<a:blip r:embed="png"/>"#;
    let cases = [
        (
            "a frame fill under the shape alt",
            pict(&rect(r#"id="a" alt="Frame alt""#, &frame("png")), ""),
            "Frame alt",
            "![Frame alt](asset:0)",
            0,
        ),
        (
            "a tile fill on an oval",
            pict(r#"<v:oval id="a" alt="logo"><v:fill r:id="png" type="tile"/></v:oval>"#, ""),
            "logo",
            "![logo](asset:0)",
            0,
        ),
        (
            "a fill under the drawing description",
            pict(&rect(r#"id="a""#, &frame("png")), described),
            "Doc descr",
            "![Doc descr](asset:0)",
            0,
        ),
        (
            "a fill alt over the drawing description",
            pict(&rect(r#"id="a" alt="Frame alt""#, &frame("png")), described),
            "Frame alt",
            "![Frame alt](asset:0)",
            0,
        ),
        (
            "a fill without alt or description",
            pict(&rect(r#"id="a""#, &frame("png")), ""),
            "Image",
            "![Image](asset:0)",
            0,
        ),
        (
            "a fill before the picture of its shape",
            pict(&rect(r#"id="a" alt="Own alt""#, &format!("{}{picture}", frame("two"))), ""),
            "Own alt",
            "![Own alt](asset:0)",
            1,
        ),
        (
            "a fill after the picture of its shape",
            pict(&rect(r#"id="a" alt="Own alt""#, &format!("{picture}{}", frame("two"))), ""),
            "Own alt",
            "![Own alt](asset:0)",
            1,
        ),
        (
            "a fill before the picture of another shape",
            pict(&format!("{}{picture_alt}", rect(r#"id="a" alt="Fill alt""#, &frame("two"))), ""),
            "Picture alt",
            "![Picture alt](asset:0)",
            1,
        ),
        (
            "a fill after the picture of another shape",
            pict(&format!("{picture_alt}{}", rect(r#"id="a" alt="Fill alt""#, &frame("two"))), ""),
            "Picture alt",
            "![Picture alt](asset:0)",
            1,
        ),
        (
            "a fill before a DrawingML picture",
            pict(&format!("{}{drawing}", rect(r#"id="a""#, &frame("two"))), ""),
            "Image",
            "![Image](asset:0)",
            1,
        ),
        (
            "a fill after a DrawingML picture",
            pict(&format!("{drawing}{}", rect(r#"id="a""#, &frame("two"))), ""),
            "Image",
            "![Image](asset:0)",
            1,
        ),
        (
            "a fill its shape switched off before the picture of its shape",
            pict(
                &rect(r#"id="a" alt="Own alt" filled="f""#, &format!("{}{picture}", frame("two"))),
                "",
            ),
            "Own alt",
            "![Own alt](asset:0)",
            0,
        ),
        (
            "a fill its shape switched off in full after the picture of its shape",
            pict(
                &rect(
                    r#"id="a" alt="Own alt" filled="false""#,
                    &format!("{picture}{}", frame("two")),
                ),
                "",
            ),
            "Own alt",
            "![Own alt](asset:0)",
            0,
        ),
        (
            "a fill its shape switched off before another shape's picture",
            pict(
                &format!(
                    "{}{picture_alt}",
                    rect(r#"id="a" alt="Fill alt" filled="f""#, &frame("two"))
                ),
                "",
            ),
            "Picture alt",
            "![Picture alt](asset:0)",
            0,
        ),
        (
            "a fill switched on under a shape switched off",
            pict(
                &rect(
                    r#"id="a" alt="Frame alt" filled="f""#,
                    r#"<v:fill r:id="png" type="frame" on="t"/>"#,
                ),
                "",
            ),
            "Frame alt",
            "![Frame alt](asset:0)",
            0,
        ),
        (
            "a fill under a shape switched on",
            pict(&rect(r#"id="a" alt="Frame alt" filled="true""#, &frame("png")), ""),
            "Frame alt",
            "![Frame alt](asset:0)",
            0,
        ),
        (
            "the first of two fills",
            pict(
                &format!(
                    "{}{}",
                    rect(r#"id="a" alt="First alt""#, &frame("png")),
                    rect(r#"id="b" alt="Second alt""#, &frame("two"))
                ),
                "",
            ),
            "First alt",
            "![First alt](asset:0)",
            1,
        ),
    ];
    let bytes: Vec<Vec<u8>> = vec![b"png".to_vec()];
    for (case, content, ..) in &cases {
        assert_eq!(shown_bytes(Format::Docx, content), bytes, "{case}");
    }
    assert_labelled(&cases, 0);
}

fn declined_choice(inner: &str) -> String {
    format!(
        r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml"><mc:Choice Requires="x">{inner}</mc:Choice><mc:Fallback/></mc:AlternateContent>"#
    )
}

fn declined_vml(format: Format, shapes: &str) -> Vec<u8> {
    match format {
        Format::Docx => docx_run(&declined_choice(&format!("<w:pict>{shapes}</w:pict>"))),
        _ => pptx(&declined_choice(shapes), &[]),
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of declined VML fills by their shapes")]
fn a_declined_vml_fill_is_a_lost_picture_only_while_its_shape_shows_it() {
    use DiagnosticCode::ImageUnplaced;
    let element = |name: &str, attributes: &str, inner: &str| {
        format!("<v:{name} {attributes}>{inner}</v:{name}>")
    };
    let rect = |attributes: &str, inner: &str| element("rect", attributes, inner);
    let fill = |rel: &str, attributes: &str| format!(r#"<v:fill r:id="{rel}" {attributes}/>"#);
    let frame = fill("png", r#"type="frame""#);
    let decoy = rect(r#"filled="f""#, &fill("two", r#"type="frame""#));
    let cases = [
        ("a fill of a shape left unswitched", rect("", &frame), 1),
        ("a fill its shape switched off", rect(r#"filled="f""#, &frame), 0),
        (
            "a fill its shape switched off in full",
            element("oval", r#"filled="false""#, &fill("png", r#"type="tile""#)),
            0,
        ),
        ("a fill its shape switched on", rect(r#"filled="t""#, &frame), 1),
        ("a fill held by the choice", frame.clone(), 1),
        ("a fill held by the choice switched off", fill("png", r#"type="frame" on="f""#), 0),
        (
            "a fill of a group in a shape switched off",
            rect(r#"filled="f""#, &element("group", "", &frame)),
            0,
        ),
        (
            "a fill of a shape switched on in a shape switched off",
            rect(r#"filled="f""#, &element("oval", r#"filled="t""#, &frame)),
            1,
        ),
        (
            "a fill of a shape switched off in a shape left unswitched",
            rect("", &element("oval", r#"filled="f""#, &frame)),
            0,
        ),
        (
            "a fill switched on under a shape switched off",
            rect(r#"filled="f""#, &fill("png", r#"type="frame" on="t""#)),
            1,
        ),
        (
            "a fill switched off under a shape switched on",
            rect(r#"filled="t""#, &fill("png", r#"type="frame" on="f""#)),
            0,
        ),
        ("a fill of a group switched off", element("group", r#"filled="f""#, &frame), 1),
        (
            "a picture of a shape switched off",
            element("shape", r#"filled="f""#, r#"<v:imagedata r:id="png"/>"#),
            1,
        ),
        (
            "a fill of a shape whose type is switched off",
            format!(r#"<v:shapetype id="t" filled="f"/>{}"#, rect(r##"type="#t""##, &frame)),
            1,
        ),
        ("a shape switched off before a fill", format!("{decoy}{}", rect("", &frame)), 1),
        ("a fill before a shape switched off", format!("{}{decoy}", rect("", &frame)), 1),
    ];
    for format in [Format::Docx, Format::Pptx] {
        for (case, shapes, unplaced) in &cases {
            let bytes = declined_vml(format, shapes);
            let found = object_outcome(format, &bytes, ImagePolicy::Placeholder).1;
            let expected = if *unplaced > 0 { vec![(ImageUnplaced, *unplaced)] } else { vec![] };
            assert_eq!(found, expected, "{case} {format:?}");
        }
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of declined VML fills by their switches")]
fn a_declined_vml_fill_in_a_shape_is_a_lost_picture() {
    use DiagnosticCode::{ImagePlaceholder, ImageUnplaced};
    let fill = r#"<v:fill r:id="png" type="frame"/>"#;
    let alternate = |chosen: &str, fallback: &str| {
        format!(
            r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml"><mc:Choice Requires="x">{chosen}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent>"#
        )
    };
    let picture = r#"<v:imagedata r:id="two"/>"#;
    let text_box = |inner: &str| {
        format!(
            "<v:textbox><w:txbxContent><w:p><w:r><w:pict>{inner}</w:pict></w:r></w:p></w:txbxContent></v:textbox>"
        )
    };
    let unplaced = vec![(ImageUnplaced, 1)];
    let cases = [
        ("a shape of no switch", "", declined_choice(fill), unplaced.clone()),
        ("a shape switched on", r#"filled="t""#, declined_choice(fill), unplaced.clone()),
        ("a shape switched off", r#"filled="f""#, declined_choice(fill), vec![]),
        ("a shape switched off in full", r#"filled="false""#, declined_choice(fill), vec![]),
        (
            "a fill switched on in a shape switched off",
            r#"filled="f""#,
            declined_choice(r#"<v:fill r:id="png" type="frame" on="t"/>"#),
            unplaced.clone(),
        ),
        (
            "a fill in its own shape in a shape switched off",
            r#"filled="f""#,
            declined_choice(&format!("<v:rect>{fill}</v:rect>")),
            unplaced.clone(),
        ),
        (
            "a fill in a text box in a shape switched off",
            r#"filled="f""#,
            declined_choice(&text_box(fill)),
            unplaced.clone(),
        ),
        (
            "a fill in a text box of a chosen shape switched off",
            "",
            declined_choice(&format!(r#"<v:rect filled="f">{}</v:rect>"#, text_box(fill))),
            unplaced.clone(),
        ),
        (
            "a fill in a text box in its own shape switched off",
            "",
            declined_choice(&text_box(&format!(r#"<v:rect filled="f">{fill}</v:rect>"#))),
            vec![],
        ),
        (
            "a picture over a fallback fill",
            "",
            alternate(picture, fill),
            vec![(ImagePlaceholder, 1)],
        ),
        (
            "a picture over a fallback fill in a shape switched off",
            r#"filled="f""#,
            alternate(picture, fill),
            unplaced,
        ),
    ];
    for (case, attributes, content, expected) in &cases {
        let bytes = docx_run(&format!("<w:pict><v:rect {attributes}>{content}</v:rect></w:pict>"));
        let found = object_outcome(Format::Docx, &bytes, ImagePolicy::Placeholder).1;
        assert_eq!(&found, expected, "{case}");
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of object labels by shape")]
fn an_object_preview_is_labelled_by_the_alt_of_its_paired_shape() {
    let shape = |id: &str, alt: &str, rel: Option<&str>| {
        vml_shape("shape", &format!(r#"id="{id}" alt="{alt}""#), rel)
    };
    let rect = |id: &str, alt: &str, rel: Option<&str>| {
        vml_shape("rect", &format!(r#"id="{id}" alt="{alt}""#), rel)
    };
    let object = |shapes: &[String], shape_id: &str, descr: &str| {
        docx_drawn(&shapes.concat(), shape_id, descr, "")
    };
    let s1 = r#"ShapeID="s1""#;
    let declined = |chosen: &str, other: &str| {
        format!(
            r#"<mc:AlternateContent><mc:Choice Requires="x">{chosen}</mc:Choice><mc:Fallback>{other}</mc:Fallback></mc:AlternateContent>"#
        )
    };
    let cases = [
        (
            "a paired preview",
            object(&[shape("s1", "An object alt", Some("png"))], s1, ""),
            "An object alt",
            "![An object alt](asset:0)",
            0,
        ),
        (
            "a shape alt over the drawing description",
            object(&[shape("s1", "An object alt", Some("png"))], s1, "Sales table"),
            "An object alt",
            "![An object alt](asset:0)",
            0,
        ),
        (
            "a blank alt under the drawing description",
            object(&[shape("s1", " ", Some("png"))], s1, "Sales table"),
            "Sales table",
            "![Sales table](asset:0)",
            0,
        ),
        (
            "a preview after another shape's",
            object(
                &[shape("s2", "Decoy alt", Some("two")), shape("s1", "An object alt", Some("png"))],
                s1,
                "",
            ),
            "An object alt",
            "![An object alt](asset:0)",
            1,
        ),
        (
            "a preview before another shape's",
            object(
                &[shape("s1", "An object alt", Some("png")), shape("s2", "Decoy alt", Some("two"))],
                s1,
                "",
            ),
            "An object alt",
            "![An object alt](asset:0)",
            1,
        ),
        (
            "a paired preview held in a rect",
            object(&[rect("s1", "An object alt", Some("png"))], s1, ""),
            "An object alt",
            "![An object alt](asset:0)",
            0,
        ),
        (
            "a stand-in alt of a rect with no preview",
            object(&[rect("s1", "An object alt", None)], s1, ""),
            "An object alt",
            "\\[An object alt\\]",
            0,
        ),
        (
            "a stand-in rect after another shape's alt",
            object(&[shape("s2", "Decoy alt", None), rect("s1", "An object alt", None)], s1, ""),
            "An object alt",
            "\\[An object alt\\]",
            0,
        ),
        (
            "a shape alt with no preview",
            object(&[shape("s1", "An object alt", None)], s1, ""),
            "An object alt",
            "\\[An object alt\\]",
            0,
        ),
        (
            "a stand-in alt over the drawing description",
            object(&[shape("s1", "An object alt", None)], s1, "Sales table"),
            "An object alt",
            "\\[An object alt\\]",
            0,
        ),
        (
            "a blank stand-in alt",
            object(&[shape("s1", " ", None)], s1, "Sales table"),
            "Sales table",
            "\\[Sales table\\]",
            0,
        ),
        (
            "a stand-in after another shape's alt",
            object(&[shape("s2", "Decoy alt", None), shape("s1", "An object alt", None)], s1, ""),
            "An object alt",
            "\\[An object alt\\]",
            0,
        ),
        (
            "a stand-in alt of the shape another id names",
            object(&[shape("s1", "An object alt", None)], r#"ShapeID="S1""#, "Sales table"),
            "Sales table",
            "\\[Sales table\\]",
            0,
        ),
        (
            "a stand-in alt of a shape in the branch not taken",
            docx_drawn(
                &declined(&shape("s1", "Declined alt", None), &shape("s1", "Fallback alt", None)),
                s1,
                "Sales table",
                "",
            ),
            "Fallback alt",
            "\\[Fallback alt\\]",
            0,
        ),
        (
            "a stand-in alt only the branch not taken holds",
            docx_drawn(
                &declined(
                    &shape("s1", "Declined alt", None),
                    &vml_shape("shape", r#"id="s1""#, None),
                ),
                s1,
                "Sales table",
                "",
            ),
            "Sales table",
            "\\[Sales table\\]",
            0,
        ),
    ];
    assert_labelled(&cases, 1);
}

type HeldCase<'a> = (&'a str, String, &'a str, Vec<DiagnosticCode>, usize, &'a [&'a [u8]]);

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the table of what an object, a control and a movie hold"
)]
fn an_iso_object_a_control_and_a_movie_report_their_loss_beside_their_preview() {
    use DiagnosticCode::{
        EmbeddedObject, FormControlUnread, GraphicUnread, ImagePlaceholder, ImageUnplaced,
    };
    let sales = r#"<wp:docPr id="1" name="Object 1" descr="Sales table"/>"#;
    let object = |inner: &str| format!("<w:object>{sales}{inner}</w:object>");
    let pict = |inner: &str| format!("<w:pict>{sales}{inner}</w:pict>");
    let embed = |attrs: &str| format!(r#"<w:objectEmbed r:id="ole" {attrs}/>"#);
    let link = |rel: &str, attrs: &str| {
        format!(r#"<w:objectLink r:id="{rel}" w:updateMode="always" {attrs}/>"#)
    };
    let control = |attrs: &str| format!(r#"<w:control r:id="ole" w:name="CheckBox1" {attrs}/>"#);
    let movie = r#"<w:movie r:id="ole"/>"#;
    let shape_alt = |id: &str, alt: &str, rel: Option<&str>| {
        vml_shape("shape", &format!(r#"id="{id}" alt="{alt}""#), rel)
    };
    let (s1, s2) = (r#"w:shapeId="s1""#, r#"w:shapeId="s2""#);
    let decoy_first =
        format!("{}{}", vml_picture("shape", "s2", "two"), vml_picture("shape", "s1", "png"));
    let two_previews =
        format!("{}{}", vml_picture("shape", "s1", "png"), vml_picture("shape", "s2", "two"));
    let preview = "![Sales table](asset:0)";
    let stand_in = "\\[Sales table\\]";
    let png: &[&[u8]] = &[b"png"];
    let cases: Vec<HeldCase<'_>> = vec![
        (
            "an embedded object over its drawing",
            object(&format!("{}{}", drawn("png"), embed(""))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder],
            1,
            png,
        ),
        (
            "a linked object over its drawing",
            object(&format!("{}{}", drawn("png"), link("ole", ""))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder],
            1,
            png,
        ),
        (
            "a linked object of an external target",
            object(&format!("{}{}", drawn("png"), link("linked", ""))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder],
            0,
            png,
        ),
        (
            "an embedded object whose shape id names a VML shape",
            object(&format!("{}{}{}", drawn("two"), vml_picture("shape", "s1", "png"), embed(s1))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder, ImageUnplaced],
            1,
            png,
        ),
        (
            "a linked object whose shape id names a VML shape",
            object(&format!(
                "{}{}{}",
                drawn("two"),
                vml_picture("shape", "s1", "png"),
                link("ole", s1)
            )),
            preview,
            vec![EmbeddedObject, ImagePlaceholder, ImageUnplaced],
            1,
            png,
        ),
        (
            "an object whose shape id follows a decoy shape",
            object(&format!("{decoy_first}{}", embed(s1))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder, ImageUnplaced],
            1,
            png,
        ),
        (
            "an object whose shape id names none",
            object(&format!("{}{}", drawn("png"), embed(r#"w:shapeId="s9""#))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder],
            1,
            png,
        ),
        (
            "an object whose shape holds no picture",
            object(&format!("{}{}{}", drawn("png"), shape_alt("s1", "Empty alt", None), embed(s1))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder],
            1,
            png,
        ),
        (
            "an object with a lowercase shape id",
            object(&format!(
                "{}{}{}",
                vml_picture("shape", "s1", "two"),
                drawn("png"),
                embed(r#"w:shapeid="s1""#)
            )),
            preview,
            vec![EmbeddedObject, ImagePlaceholder, ImageUnplaced],
            1,
            png,
        ),
        (
            "an object beside an id-less shape before its drawing",
            object(&format!(
                "{}{}{}",
                vml_shape("shape", r#"alt="Idless alt""#, Some("two")),
                drawn("png"),
                embed("")
            )),
            preview,
            vec![EmbeddedObject, ImagePlaceholder, ImageUnplaced],
            1,
            png,
        ),
        (
            "an object labelled by the alt of the shape its shape id names",
            object(&format!("{}{}", shape_alt("s1", "Shape alt", Some("png")), embed(s1))),
            "![Shape alt](asset:0)",
            vec![EmbeddedObject, ImagePlaceholder],
            1,
            png,
        ),
        (
            "a stand-in labelled by the alt of the shape its shape id names",
            object(&format!(
                "{}{}{}",
                shape_alt("s2", "Decoy alt", None),
                shape_alt("s1", "Shape alt", None),
                embed(s1)
            )),
            "\\[Shape alt\\]",
            vec![EmbeddedObject, ImagePlaceholder],
            1,
            &[],
        ),
        (
            "an object beside two drawings",
            object(&format!("{}{}{}", drawn("png"), drawn("two"), embed(""))),
            preview,
            vec![EmbeddedObject, ImagePlaceholder, ImageUnplaced],
            1,
            png,
        ),
        (
            "a stand-in labelled by the first shape its shape id names",
            object(&format!(
                "{}{}{}",
                shape_alt("s1", "First alt", None),
                shape_alt("s1", "Second alt", None),
                embed(s1)
            )),
            "\\[First alt\\]",
            vec![EmbeddedObject, ImagePlaceholder],
            1,
            &[],
        ),
        (
            "the preview of an object that is not read",
            object(&format!("{two_previews}{}{}", embed(s1), embed(s2))),
            preview,
            vec![EmbeddedObject, EmbeddedObject, ImagePlaceholder],
            1,
            png,
        ),
        (
            "a control over its shape",
            object(&format!("{decoy_first}{}", control(r#"w:shapeid="s1""#))),
            preview,
            vec![FormControlUnread, ImagePlaceholder, ImageUnplaced],
            0,
            png,
        ),
        (
            "a control in a pict",
            pict(&format!("{decoy_first}{}", control(r#"w:shapeid="s1""#))),
            preview,
            vec![FormControlUnread, ImagePlaceholder, ImageUnplaced],
            0,
            png,
        ),
        (
            "a stand-in labelled by the alt of the shape a control names",
            object(&format!(
                "{}{}{}",
                shape_alt("s2", "Decoy alt", None),
                shape_alt("s1", "Shape alt", None),
                control(r#"w:shapeid="s1""#)
            )),
            "\\[Shape alt\\]",
            vec![FormControlUnread, ImagePlaceholder],
            0,
            &[],
        ),
        (
            "a control with a camel-case shape id",
            object(&format!("{}{}", vml_picture("shape", "s1", "png"), control(s1))),
            stand_in,
            vec![FormControlUnread, ImagePlaceholder, ImageUnplaced],
            0,
            &[],
        ),
        (
            "a control beside its drawing",
            object(&format!("{}{}", drawn("png"), control(""))),
            preview,
            vec![FormControlUnread, ImagePlaceholder],
            0,
            png,
        ),
        (
            "the preview of a control that is not read",
            object(&format!("{two_previews}{}{}", embed(s1), control(r#"w:shapeid="s2""#))),
            preview,
            vec![EmbeddedObject, FormControlUnread, ImagePlaceholder],
            1,
            png,
        ),
        (
            "a control beside an embedded object",
            object(&format!("{}{}{}", drawn("png"), control(""), embed(""))),
            preview,
            vec![FormControlUnread, EmbeddedObject, ImagePlaceholder],
            0,
            png,
        ),
        (
            "an embedded object beside a control",
            object(&format!("{}{}{}", drawn("png"), embed(""), control(""))),
            preview,
            vec![EmbeddedObject, FormControlUnread, ImagePlaceholder],
            1,
            png,
        ),
        (
            "a movie beside its drawing",
            object(&format!("{}{movie}", drawn("png"))),
            preview,
            vec![GraphicUnread, ImagePlaceholder],
            0,
            png,
        ),
        ("a movie alone", object(movie), stand_in, vec![GraphicUnread, ImagePlaceholder], 0, &[]),
    ];
    let unseen = ("result".to_string(), Vec::new(), 0);
    for (case, content, markdown, codes, payloads, shown) in cases {
        let mut found = std::collections::BTreeMap::new();
        for code in codes {
            *found.entry(code).or_insert(0u64) += 1;
        }
        let bytes = docx_run(&content);
        let outcome = object_outcome(Format::Docx, &bytes, ImagePolicy::Placeholder);
        let found: Vec<(DiagnosticCode, u64)> = found.into_iter().collect();
        let expected = (markdown.to_string(), found, payloads);
        assert_eq!(outcome, expected, "{case}");
        assert_eq!(shown_bytes(Format::Docx, &bytes), shown, "{case}");
        let outcome =
            object_outcome(Format::Docx, &in_instruction(&content), ImagePolicy::Placeholder);
        assert_eq!(outcome, unseen, "{case} in an instruction");
    }
}
