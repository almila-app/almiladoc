# Fuzzing

    make tools
    cd fuzz
    cargo fuzz run --target x86_64-unknown-linux-gnu xlsx corpus/xlsx seeds/xlsx

- Run here under the pinned nightly with `$CARGO_TARGET_DIR/make/tools/cargo-fuzz-<version>` (else `target/make/tools/...`) on PATH; the musl binary needs an explicit host `--target`.
- `make fuzz` builds every target/feature and replays seeds/regressions once; each target needs a non-dot seed and a regressions directory (`.gitkeep` until a finding).
- `seeds/` holds fixture-derived starting inputs, `regressions/<target>/` released crash reproducers, and untracked `corpus/` libFuzzer's working inputs.
- `make locks` reproduces tracked Cargo.lock from root/shared versions plus fuzz-only pins and fails differences; refresh by copying root Cargo.lock here and running `cargo metadata --format-version 1` once.
- XLS/XLSB/numfmt wrap mutations in OLE/OPC/styles containers to reach inner parsers; XLSX consumes whole workbooks, seeding all three containers because detection chooses by bytes.
- Shared config lowers max_grid_slots to fit the slow-unit bound; whole-input convert and part-injection check select all 16 comment/hidden/revision/header-footer profiles via stable FNV-1a of unchanged original bytes, including aligned CFB inputs. Saved inputs retain profiles across toolchains; seed contract tests explicitly use defaults and render/paint targets retain separate configs.
- pdf_raster forces every page to a gap through painting_converter(), also lowering max_work, and compares two paints at ≤50 dpi/1 Mpx.
- docx_body/pptx_slide/xlsx_sheet/odf_content inject one part into a stored, explicitly labelled package from src/lib.rs, bypassing detection on mutated bytes; compare two conversions and two parse/JSON results with one converter. Fixture-part seeds are tested for expected diagnostics/Markdown; ODT body selects text/sheet/presentation.
- Optional dicts/<target>.dict declares every prefix in its matched prefix/local pairs; cargo test rejects undeclared prefixes and words libFuzzer would reject or drop beyond 64 bytes.

## Campaigns

Scheduled workflows run 30 minutes/target daily plus random-seed proptests, and minimize corpora weekly via `make fuzz-campaign` (`.github/scripts/fuzz_campaign.py`). Public logs expose only target, failure kind and input SHA-256 (log SHA-256 without input); input/log/seed upload only encrypted to recipients.txt as .age, and only passing runs save the PR-readable corpus cache; dispatch drill=<target> seals its first seed to prove recovery.

Decrypt and minimize:

    age -d -i <identity> -o input <target>-<kind>-<sha256>.age
    age -d -i <identity> <target>.log.age
    cargo fuzz tmin --target x86_64-unknown-linux-gnu <target> input

Fix with a regression failing before the change; publish the input in regressions/<target>/ only after a release includes the fix. Proptest findings seal their generated proptest-regressions file as proptest-failure-<sha256>.age; restore it at the decrypted log's path for cargo test to replay first.
