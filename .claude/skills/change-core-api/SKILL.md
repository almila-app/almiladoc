---
name: change-core-api
description: Change the public surface the Python binding mirrors - `Format`, a model type or
  field, `ConvertError` or its codes, or a public function - and carry it into the binding. Use
  before editing src/lib.rs, src/error.rs or src/model/, or when the binding breaks after one.
---

# Change the core API

The Python binding hand-mirrors the core, and the compiler catches none of its drift. A new
`Block`, `Inline` or `ConvertError` variant (each `#[non_exhaustive]`) reaches the binding's
wildcard arm and raises `InternalError`; a new model field is missed, since the binding builds
its own classes from the core's fields by name; a `Format` absent from `FORMATS` in
`python/src/lib.rs` makes its name a `ValueError`; and nothing compiles the stub against the core.

1. Change the core and its tests. A new error variant gets its `code()` arm and an assertion in
   `codes_name_every_variant`, which lists codes by hand. Codes ship in snake_case and, once
   released, are never renamed. A variant's fields reach Python as exception attributes (`pages`,
   `page_count` for `NeedsOcr`). A new `Style` flag changes Markdown output: the renderer merges
   runs by `Style` equality and escapes by comparison with `Style::PLAIN`
   (`src/render/markdown/inline.rs`).
2. Mirror it in the binding:
   - `python/src/lib.rs`: per error variant an exception class, its arm in `convert_error` with
     its attributes, and its `m.add`; per format a `FORMATS` row.
   - `python/src/document.rs`: per model type a class, per core field a field.
   - `python/almiladoc/_almiladoc.pyi` and `__init__.py`: imports, `__all__`, and in
     `__init__.py` the `Format` Literal, which the stub imports. `make python` runs stubtest
     against the built module; the suite checks that stub and module export the same names.
3. A new format also needs `Format::from_extension`, a detection rule in `src/formats/detect.rs`,
   a fixture directory (`/add-fixture`), and a fuzz target with its `[[bin]]` in
   `fuzz/Cargo.toml`, seeds in `fuzz/seeds/<target>/` and a `fuzz/regressions/<target>/`
   directory: `make fuzz` fails without either.
4. Update README.md where it lists formats, error variants or exception classes.
5. Run `.claude/skills/change-core-api/scripts/mirror_drift.py` over the working tree: it names
   each struct field the binding's own type does not declare, and every other core identifier
   the binding never spells. Then `/verify`.
