use std::io::{Cursor, Write};

pub(crate) fn zip_package<N: AsRef<str>, B: AsRef<[u8]>>(parts: &[(N, B)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in parts {
        writer.start_file(name.as_ref(), zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(body.as_ref()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
