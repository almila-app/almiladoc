---
paths:
  - "**/*.py"
  - "**/*.pyi"
  - "python/**"
---

# Python

- `make hygiene` runs `ruff format --check` and `ruff check` under pyproject.toml's
  `[tool.ruff]` (PEP 8 at 99 columns).
- Annotate as `ANN` requires, bar `[tool.ruff]` per-file ignores; `_almiladoc.pyi` passes stubtest.
- Use `pathlib.Path`, f-strings and `zip(..., strict=True)`; never assign a lambda to a name.
- The binding converts types and errors only, behaviour lives in the crate, and a mirrored type
  changes through `/change-core-api`.
- A fixture generator builds each part or record in one named function, names spec ids as
  constants and writes the same bytes on every run (`fixtures.py check`).
- A test asserts exact values and exception classes, never truthiness alone.
