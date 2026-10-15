# Fuzzing

    cd fuzz
    cargo fuzz run xlsx corpus/xlsx seeds/xlsx

`rust-toolchain.toml` here pins the nightly that cargo-fuzz needs; run it from this directory.

`fuzz/seeds/` holds checked-in starting inputs; `fuzz/corpus/` is the working
directory libfuzzer writes to and is not checked in.

`xls`, `xlsb` and `numfmt` wrap their input in a valid container (an OLE
compound file, an OPC package, a styles part) so mutation reaches the record
and format-code parsers rather than dying at the container gate. `xlsx` takes
a whole workbook, and its seeds cover all three containers because the
frontend picks the reader from the bytes.
