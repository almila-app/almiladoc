#![no_main]

use almiladoc::Format;
use almiladoc_fuzz::{convert, fuzz_target};
use std::io::{Cursor, Write};

// The BIFF record stream, wrapped in a valid OLE container so mutation
// reaches the reader instead of dying at the container gate.
fuzz_target!(|data: &[u8]| {
    let Ok(mut ole) = cfb::CompoundFile::create(Cursor::new(Vec::new())) else {
        return;
    };
    if ole.create_stream("Workbook").and_then(|mut stream| stream.write_all(data)).is_err() {
        return;
    }
    convert(&ole.into_inner().into_inner(), Format::Excel);
});
