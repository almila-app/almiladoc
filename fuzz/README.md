# Fuzzing

    make tools
    cd fuzz
    cargo fuzz run --target x86_64-unknown-linux-gnu xlsx corpus/xlsx seeds/xlsx

`rust-toolchain.toml` here pins the nightly that cargo-fuzz needs; run it from this directory,
with `$CARGO_TARGET_DIR/make/tools/cargo-fuzz-<version>` (or `target/make/tools/...`) on `PATH`.
That binary is a musl build: name the host's `--target`. `make fuzz` is the gate that builds every
target and replays its seeds and regressions once; it fails a target whose `seeds/<target>/` holds
no file with a name that lacks a leading dot, or whose `regressions/<target>/` is missing, so a new
target brings a real seed and a `.gitkeep` in `regressions/<target>/` until a crash input lands.

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

Every target converts through `converter()` in `src/lib.rs`, which the whole-input targets reach as
`convert(data, format)`: the default configuration with `max_grid_slots` lowered, so a grid at its
limit stays inside libFuzzer's slow-unit bound.

`docx_body`, `pptx_slide`, `xlsx_sheet` and `odf_content` write the input as the one fuzzed part of a
stored package that `src/lib.rs` builds, and name its format, so detection never reads fuzzed
bytes. Each converts the package twice, and parses and serialises it to JSON twice, with one
converter, and fails on any difference. Their seeds are the parts of fixtures, and `cargo test`
here injects every seed and requires its expected diagnostics and a line of its expected
Markdown. `odf_content` is an ODT package whose body element selects the text, spreadsheet or
presentation reader. `dicts/<target>.dict` is passed to a campaign when present and
holds the (prefix, local) pairs the reader matches, each prefix declared in the dictionary;
`cargo test` fails on a word libFuzzer would reject or drop past its 64-byte limit, or an
undeclared prefix.

## Campaigns

`.github/workflows/scheduled.yml` fuzzes every target for 30 minutes a day from its cached corpus,
runs the library's proptests under a random seed, and minimises each corpus weekly, all through
`make fuzz-campaign` (`.github/scripts/fuzz_campaign.py`). A run's public log names only the
target, the kind of failure and a sha256: the input's, or the log's for a failure without one;
the input, the log and the seed are uploaded only as `.age` files encrypted to `recipients.txt`.
The corpus cache is readable by every pull request, so only a passing run saves it. A dispatch
with `drill` set to a target seals that target's first seed as if it had crashed, to prove the
path.

To act on a finding:

    age -d -i <identity> -o input <target>-<kind>-<sha256>.age
    age -d -i <identity> <target>.log.age
    cargo fuzz tmin --target x86_64-unknown-linux-gnu <target> input

Fix it with a test that fails without the fix, and commit the input to `regressions/<target>/`
only once a release carries the fix. A proptest failure seals the `proptest-regressions/` file
it wrote as `proptest-failure-<sha256>.age`, whose path the decrypted log names; put it back
there and `cargo test` replays it first.
