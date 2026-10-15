# 0003. CI and release

Status: accepted
Date: 2026-10-14

## Context

- An organisation ruleset requires GitHub Actions' `ci / required` on every push to `main`.
- Runners are GitHub-hosted, without organisation secrets; no allowlisted action builds Rust or wheels.

## Decision

1. **Topology.** ADR 0004 decision 4's `ci.yml` cancels superseded runs; its job `ci` calls `gates.yml`, whose `required` needs every gate. Jobs hold least privilege. Release and scheduled runs never cancel.

2. **Gates** are Makefile targets, `make ci` the pre-flight; jobs run on `ubuntu-26.04` unless named. `LICENCE_SET` is `LICENSE LICENSES/ NOTICE REUSE.toml THIRD-PARTY-LICENSES`.
   - `rust`: fmt, clippy, locked tests and docs, all features, `-D warnings`; snapshots byte-compared and never updated; proptest seeds fixed.
   - `rust-arm`: `make test` on `ubuntu-26.04-arm`, byte-comparing non-PDF snapshots only.
   - `features`: clippy and tests without default features; each feature alone.
   - `python`: the wheel built from the unpacked sdist, alone in CPython 3.11 and 3.15; pytest, stubtest, `py.typed`; a licence test checks `$(LICENCE_SET)` and `FSL-1.1-MIT AND MIT` metadata.
   - `policy`: cargo deny over the workspace, all features, and `fuzz/`; `deny-offline.toml`, beside ADR 0001's lints, bans `ureq`, `hyper`, `reqwest`, `libloading` and `rten` from the default and `render` graphs; a reproduced `fuzz/Cargo.lock`; `cargo about --workspace --all-features`, reaching pyo3, reproduces `THIRD-PARTY-LICENSES`; `reuse lint`; `hygiene`; online pedantic zizmor.
   - `dco`: `make dco provenance` (ADR 0004).
   - `fuzz`: every target builds, with each declared feature; an empty seed directory fails; seeds and `fuzz/regressions/` replay once; the docs.rs build passes.

3. **Tools.** `rust-toolchain.toml` pins the latest stable release; `rust-version` equals its minor. Cargo tools are sha256-pinned release binaries. Python tools, maturin and ziglang included, are locked `uv` groups. `ci.yml` and `release.yml` use no cache.

4. **Schedules.** `scheduled.yml` runs daily per-target fuzz campaigns and random-seed proptests, and weekly `cargo fuzz cmin` and a staleness check of pinned binaries, image digests and, while ADR 0001 decision 6's fork is in use, upstream pdf-inspector releases and advisories; it alone saves the corpus cache. Public logs name only target, exit kind and input hash; failing inputs upload only encrypted, committed as reproducers after the fix's release.

5. **OCR goldens.** Every `release.yml` run checks ADR 0002 decision 13's goldens, raster hashes and PaddleX parity on hash-checked weights; publishing needs the goldens once `ocr-local` exists.

6. **Release.** `release.yml` runs on `v*` tag pushes, which alone publish, and rehearses on dispatch and weekly. Only publish jobs and `attest` hold `id-token: write`.
   - `guard`: advisories pass; on a tag push, the Competing Use FAQ exists, the annotated tag equals the workspace version, `rust-version` the toolchain minor, its commit on `main` with green `ci / required`, and `docs/release-dates.tsv` covers earlier final tags.
   - `linux`: maturin `--zig` builds `manylinux_2_28` and `musllinux_1_2` wheels on native-arch Ubuntu runners. Smoke, in PyPA's matching digest-pinned image: CPython 3.11 and 3.15 tests, a CMap-needing PDF, the licence test; wheels under ADR 0002's 12 MB.
   - `native`: `macos-26`, `macos-15-intel`, `windows-2025`, `windows-11-arm`; same smoke.
   - `sdist`, `crate`: both carry `$(LICENCE_SET)`; the sdist's own wheel is smoked. `attest` covers every file.
   - `publish-pypi` (environment `pypi`; rc tags `testpypi`): signed attestations, trusted publishing only.
   - `publish-crates` (environment `crates-io`, final tags): `cargo publish` with an OIDC-exchanged token, revoked after use; the first final tag uses a `publish-new`-scoped environment secret, deleted after use.
   - `verify-published` checks PyPI's files and provenance publisher (workflow, environment) against attestations; crates.io's checksum against the attested `.crate` and, after the first version, its trusted-publishing repository, run and commit.
   - `github-release`: the tag message, dists and `SHA256SUMS`.

7. **Dates.** The FSL's future grant counts from "the date we make the Software available": the UTC day of the earliest repository activity entry (https://docs.github.com/en/rest/repos/repos#list-repository-activities) whose `after` contains the tagged commit. The release notes, with the day FSL-1.1-MIT portions become MIT, and later `docs/release-dates.tsv` record it.

8. **Supply chain.** Dependabot, 7-day cooldown, covers `cargo` (`/fuzz` for `libfuzzer-sys` only), `uv`, `rust-toolchain` (both) and `github-actions`. The maintainer adopts each update as an own signed-off commit (ADR 0004), moving fuzz's `cfb` and `zip` with the root and `rust-version` with the toolchain. `deny.toml`: permissive licences and ADR 0002's; exceptions FSL-1.1-MIT for `almiladoc`, the binding and fuzz crates alone, fuzz's declaring `FSL-1.1-MIT AND MIT`; NCSA for `libfuzzer-sys`; yanked crates, non-path wildcards, `openssl-sys`, `ort-sys` and `aws-lc-sys` denied; crates.io only; wheel targets.

9. **Settings** the maintainer checks before the first release:
   - `sha_pinning_required`; immutable releases; private vulnerability reporting; CodeQL default setup outside `required`.
   - Two `v*` tag rulesets, as a bypass spans its ruleset (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/creating-rulesets-for-a-repository.md#L23): creation, admin-bypassable only; update and deletion, unbypassable.
   - Environments `pypi`, `testpypi`, `crates-io`: maintainer-reviewed, `v*` tags only, no admin bypass.
   - Trusted publishers bind `release.yml` and the environment, optional in both registries; pending PyPI publishers precede the first rc tag.
   - Trusted publishing cannot create a crate (https://github.com/rust-lang/crates.io/blob/e15f258b77b9e73dded45339bb695960f660d33b/src/controllers/krate/publish.rs#L190-L195): the maintainer adds the trusted publisher and `trustpub_only` after decision 6's first token publish. `almiladoc-pdf-inspector`, if ADR 0001 decision 6's fork is needed, is published from its own repository by the same bootstrap before any `almiladoc` candidate depends on it.

## Consequences

- The first candidate owes every gate input: `THIRD-PARTY-LICENSES`, with cargo-about clarifications for Adobe's BSD-3-Clause CMaps (ADR 0001 decision 6), eight fuzz seeds, a tracked `fuzz/Cargo.lock`; schedules, the recipient key and a decryption drill; the first tag, the Competing Use FAQ; `ocr-http`, ring's clarification.
- New advisories red unrelated pull requests; schedules stop after 60 inactive days.
- Unexercised: zig musl builds, unguaranteed by maturin; ring's C build under zig and on `windows-11-arm`, which needs clang and setup-python's CPython 3.11.9, the last arm64 build (https://github.com/actions/python-versions/blob/49858b91c92d27403416ee43117032e91d00ce24/versions-manifest.json#L10809-L10812); attestations; crates.io's exchange.

## Alternatives considered

- A plain job `ci / required`: never seen satisfying this ruleset.
- Offline zizmor: impostor commits pass.
- Gates joining `required` stepwise: earlier candidates land unjudged.
- PyPA build containers: no Rust; Dockerfiles to maintain.
- Network namespaces around tests: sudo, for exercised paths only.
- An ambient-read scanner: a custom script.
- Signed tags: rulesets and environment approval suffice.
- Gated reproducibility: double builds.
- A settings probe: a script for one reading.
- `group-by: dependency-name` across `/fuzz`: spans only shared dependencies.
- Encrypting the pull-request-readable corpus cache (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/actions/reference/workflows-and-actions/dependency-caching.md#L243): needs a secret; crash inputs never enter it (https://github.com/llvm/llvm-project/blob/fa24f6dba7cd2e71b665af642c19b0f879fe2279/llvm/docs/LibFuzzer.md#L146-L154).
- `published_at` as the date: candidates publish source earlier.
