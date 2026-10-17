# Fuzzing

    make tools
    cd fuzz
    cargo fuzz run --target x86_64-unknown-linux-gnu xlsx corpus/xlsx seeds/xlsx

`rust-toolchain.toml` here pins the nightly that cargo-fuzz needs; run it from this directory,
with `$CARGO_TARGET_DIR/make/tools/cargo-fuzz-<version>` (or `target/make/tools/...`) on `PATH`.
That binary is a musl build: name the host's `--target`. `make fuzz` runs the campaign script's
self-test, builds every target, replays `seeds/<target>` and `regressions/<target>` once, fails on
a missing regression directory or a missing or empty seed directory, and builds the docs as
docs.rs does.

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
