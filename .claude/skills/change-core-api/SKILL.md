---
name: change-core-api
description: Change the public surface the Python binding mirrors - `Format`, a model type or
  field, `ConvertError` or its codes, `Config`, a `Limits` field, `OnLoss`, `Diagnostic`,
  `OcrRequest`, `OcrHttpConfig` or a public `Converter`, `Parsed` or `OcrHttp` call - and carry it
  into the binding. Use before editing src/lib.rs, src/error.rs, src/config.rs, src/limits.rs,
  src/converter.rs, src/diagnostics.rs, src/model/ or src/ocr/, or when the binding breaks after
  one.
---

# Change the core API

The model reaches Python only as the JSON `Parsed::to_json` writes, which serde derives from the
core types. A format name reaches the binding through `Format`'s `FromStr`. The rest of the
binding is hand-mirrored, and the compiler catches none of its drift: a new `ConvertError` variant
(`#[non_exhaustive]`) reaches the binding's wildcard arm and raises `InternalError`, a new
`Limits` field is unreachable from Python until the binding lists it, and nothing compiles the
stub against the core.

1. Change the core and its tests. A new public entry point lands with its compiled rustdoc
   example, run by `cargo test --doc`. A new error variant gets its `code()` arm and an assertion in
   `codes_name_every_variant`, which lists codes by hand; a new `UnsupportedKind` gets its
   `as_str` arm and its row in `unsupported_kinds_name_every_kind`. Codes and kind names ship in
   snake_case and, once released, are never renamed. A variant's fields reach Python as exception
   attributes (`pages`, `page_count` for `NeedsOcr`). A new `Style` flag changes Markdown output:
   the renderer merges runs by `Style` equality and escapes by comparison with `Style::PLAIN`
   (`src/render/markdown/inline.rs`).
   A model change changes the JSON: review the wire snapshot in `src/json.rs`, and raise
   `JSON_SCHEMA_VERSION` only when a field or `kind` is removed, renamed or changes meaning.
2. Mirror it in the binding (`python/src/`):
   - `errors.rs`: per error variant an exception class and its arm in `raise` with its
     attributes; per `ConfigError` variant its arm in `config_error`. `lib.rs`: a new class,
     function or exception joins a `#[pymodule_export]` import.
   - `config.rs`: a `Limits` field joins the `limits!` list with its one-line doc; a `Config`
     field gets its keyword, getter, `__repr__` and `Hash` term; an `OnLoss` variant its
     `#[classattr]` and `__repr__` arm.
   - `ocr.rs`: an `OcrRequest` field, as a getter and in `__reduce__`'s arguments; an
     `OcrHttpConfig` field, as an `OcrHttp` keyword; an `OcrSummary` field, as a getter; an
     `OcrOutcome` or `GapFill` variant, as its arm in `answer`.
   - `converter.rs`: a public `Converter` or `Parsed` call, run detached; `input.rs` admits every
     input against `max_input_bytes` before it is copied or read. `diagnostics.rs`: a
     `Diagnostic` field's getter.
   - `python/almiladoc/_almiladoc.pyi` and `__init__.py`: imports, `__all__`, and in
     `__init__.py` the `Format` Literal, which the stub imports. `make python` runs stubtest
     against the built module; the suite checks that stub and module export the same names and
     that every public name has a docstring. A pyo3 `Vec<T>` parameter takes only a sequence, so
     the stub says `Sequence`; a core doc that narrows a claim narrows each docstring repeating it.
3. A new format also needs its `Format::as_str` arm and its `Format::ALL` entry (the tests in
   `src/lib.rs` fail without either), `Format::from_extension`, a detection rule in
   `src/formats/detect.rs`, a fixture directory (`/add-fixture`), and a fuzz target with its
   `[[bin]]` in `fuzz/Cargo.toml`, seeds in `fuzz/seeds/<target>/` and a
   `fuzz/regressions/<target>/` directory: `make fuzz` fails without either.
4. Update README.md where it lists formats, error variants, exception classes or the Python API;
   `make python` runs its first `python` block.
5. Run `.claude/skills/change-core-api/scripts/mirror_drift.py` over the working tree: it names
   each format name the `Format` Literal lacks, each exception the binding never spells, each
   `Limits` field the binding or stub never names, and each `code()` or
   `UnsupportedKind::as_str` string that is not snake_case. Then `/verify`.
