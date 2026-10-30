---
paths:
  - "**/*.py"
  - "**/*.pyi"
  - "python/**"
  - "pyproject.toml"
---

# Python

`pyproject.toml`'s `[tool.ruff]` owns enforced Python style; `make hygiene` runs it.

- The binding converts types and errors only, behaviour lives in the crate, and a mirrored type
  changes through the `change-core-api` workflow.
- A fixture generator builds each part or record in one named function, names spec ids as
  constants and writes the same bytes on every run (`fixtures.py check`).
- A test asserts exact values and exception classes, never truthiness alone.
- A word check bounds words by letters, not `\b`, as `_` and digits join words in logins and
  addresses; its self-test holds a case per word and per field searched.
- Never put `[tool.uv] required-version`, or any setting that binds every uv call, in
  `pyproject.toml`: it breaks the maintainer's tooling run inside the checkout. Pin uv where the
  gates run (the Makefile's `UV_VERSION`, `setup-uv`'s `version`).
