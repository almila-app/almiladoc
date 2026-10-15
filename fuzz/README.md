# Fuzzing

    make tools
    cd fuzz
    cargo fuzz run --target x86_64-unknown-linux-gnu xlsx corpus/xlsx seeds/xlsx

`rust-toolchain.toml` here pins the nightly that cargo-fuzz needs; run it from this directory,
with `$CARGO_TARGET_DIR/make/tools/cargo-fuzz-<version>` (or `target/make/tools/...`) on `PATH`.
That binary is a musl build: name the host's `--target`. `make fuzz` builds every target, replays
`seeds/<target>` and `regressions/<target>` once, fails on a missing regression directory or a
missing or empty seed directory, and
builds the docs as docs.rs does.

`seeds/` holds checked-in starting inputs, byte copies of fixtures where a format has them;
`regressions/<target>/` holds inputs that once crashed; `corpus/` is the working directory
libfuzzer writes to and is not checked in.

`Cargo.lock` is tracked; `make locks` reproduces it from the root lockfile plus its own fuzz-only
pins, so every shared package keeps the root's version, and fails on any difference. To refresh it, copy the root
`Cargo.lock` here and run `cargo metadata --format-version 1` once.

`xls`, `xlsb` and `numfmt` wrap their input in a valid container (an OLE
compound file, an OPC package, a styles part) so mutation reaches the record
and format-code parsers rather than dying at the container gate. `xlsx` takes
a whole workbook, and its seeds cover all three containers because the
frontend picks the reader from the bytes.
