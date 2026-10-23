use crate::build::{atom, container, record};
use crate::support::{POLICIES, configured, reported};
use almiladoc::{DiagnosticCode, Format, ImagePolicy, Input, OnLoss, Region};
use std::io::{Cursor, Write};

fn pages(instance: u16, values: &[(u32, u32)]) -> Vec<u8> {
    container(
        instance,
        0x0ff0,
        &values
            .iter()
            .map(|&(persist, id)| atom(0x03f3, &[persist, 0, 0, id, 0]))
            .collect::<Vec<_>>(),
    )
}
fn drawing() -> Vec<u8> {
    let picture = container(
        0,
        0xf004,
        &[
            record(2, 0xf00a, &[&1u32.to_le_bytes()[..], &0x0a00u32.to_le_bytes()].concat()),
            record(0x13, 0xf00b, &[&0x4104u16.to_le_bytes()[..], &1u32.to_le_bytes()].concat()),
        ],
    );
    container(0, 0x040c, &[container(0, 0xf002, &[container(0, 0xf003, &[picture])])])
}
fn slide(picture: bool) -> Vec<u8> {
    let mut content = vec![atom(0x03ef, &[0; 6]), record(0, 0x0fa8, b"keep\r")];
    if picture {
        content.push(drawing());
    }
    container(0, 0x03ee, &content)
}
fn store() -> Vec<u8> {
    let entry = record(
        0x0062,
        0xf007,
        &[&[0; 24][..], &1u32.to_le_bytes(), &0u32.to_le_bytes(), &[0; 4]].concat(),
    );
    container(0, 0x040b, &[container(0, 0xf000, &[container(1, 0xf001, &[entry])])])
}
fn deck(document: &[Vec<u8>], objects: &[Vec<u8>], unread: bool) -> Vec<u8> {
    let document = container(0, 0x03e8, document);
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    for object in std::iter::once(&document).chain(objects.iter()) {
        offsets.push(data.len() as u32);
        data.extend(object);
    }
    let count = offsets.len() as u32;
    let directory = data.len() as u32;
    data.extend(atom(0x1772, &[&[1 | (count << 20)][..], &offsets].concat()));
    let edit = data.len() as u32;
    data.extend(atom(0x0ff5, &[0, 0, 0, directory, 1, count + 1, 0]));
    let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    ole.create_stream("Current User")
        .unwrap()
        .write_all(&atom(0x0ff6, &[20, 0xe391_c05f, edit]))
        .unwrap();
    ole.create_stream("PowerPoint Document").unwrap().write_all(&data).unwrap();
    if unread {
        ole.create_storage("Pictures").unwrap();
    } else {
        ole.create_stream("Pictures").unwrap().write_all(&record(0x6e00, 0xf01e, &[0; 4])).unwrap();
    }
    ole.into_inner().into_inner()
}

#[test]
fn picture_stream_failures_report_only_owned_region_and_unit_uses() {
    let note = container(0, 0x03f0, &[atom(0x03f1, &[256, 0]), drawing()]);
    let cases = [
        (deck(&[pages(0, &[(2, 256)])], &[slide(false)], true), Vec::new()),
        (
            deck(&[pages(0, &[(2, 256), (3, 257)]), store()], &[slide(true), slide(true)], false),
            vec![(Region::Body, Some(1), 1), (Region::Body, Some(2), 1)],
        ),
        (
            deck(
                &[pages(0, &[(2, 256)]), pages(2, &[(3, 1)]), store()],
                &[slide(false), note],
                false,
            ),
            vec![(Region::Note, Some(1), 1)],
        ),
        (
            deck(&[pages(0, &[(2, 256)]), store()], &[slide(true)], true),
            vec![(Region::Body, Some(1), 1)],
        ),
    ];
    for (bytes, expected) in cases {
        for images in POLICIES {
            let converter = configured(|config| {
                config.images = images;
                config.on_loss = OnLoss::Report;
            });
            let result = converter.convert(Input::new(&bytes, Format::Ppt)).unwrap();
            assert_eq!(
                result,
                converter.parse(Input::new(&bytes, Format::Ppt)).unwrap().render().unwrap()
            );
            let code = if images == ImagePolicy::Placeholder {
                DiagnosticCode::ImageUnavailable
            } else {
                DiagnosticCode::ImageOmitted
            };
            let coded: Vec<_> =
                expected.iter().map(|&(region, unit, count)| (code, region, unit, count)).collect();
            assert_eq!(reported(result.diagnostics()), coded);
            let strict = configured(|config| config.images = images)
                .convert(Input::new(&bytes, Format::Ppt));
            assert_eq!(strict.is_err(), images == ImagePolicy::Placeholder && !expected.is_empty());
        }
    }
}
