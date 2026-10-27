# 0003. CI and release

Status: accepted
Date: 2026-10-14, revised 2026-10-27

## Context

- An organisation ruleset requires GitHub Actions' `ci / required` on every push to `main`.
- Runners are GitHub-hosted, without organisation secrets; no allowlisted action builds Rust or wheels.

## Decision

1. **Topology.** ADR 0004 decision 4's `ci.yml` cancels superseded runs; its job `ci` calls `gates.yml`, whose `required` needs every gate. `cache.yml`, on each push to `main` and on dispatch, cancels superseded runs and calls `gates.yml` with `warm`, which skips `policy` and `required` (decision 3). Jobs hold least privilege. Release and scheduled runs never cancel.

2. **Gates** are Makefile targets, `make ci` the pre-flight; jobs run on `ubuntu-26.04` unless named. `LICENCE_SET` is `LICENSE LICENSES/ NOTICE REUSE.toml THIRD-PARTY-LICENSES`.
   - `rust`: fmt, clippy, locked tests and docs, all features, `-D warnings`; snapshots byte-compared and never updated; proptest seeds fixed.
   - `rust-arm`: `make test` on `ubuntu-26.04-arm`, byte-comparing non-PDF snapshots only.
   - `features`: clippy of `almiladoc` and its tests without default features (a workspace build takes the binding's features, so only `-p almiladoc` builds without them); each feature alone.
   - `python`: the wheel built from the unpacked sdist, alone in CPython 3.11 and 3.15; pytest, stubtest, `py.typed`; a licence test checks `$(LICENCE_SET)` and `BUSL-1.1 AND MIT` metadata.
   - `policy`: cargo deny over the workspace, all features, and `fuzz/`; `deny-offline.toml`, beside ADR 0001's lints, bans `ureq` but as `almiladoc`'s, which its `ocr-http` takes, and `hyper`, `reqwest`, `libloading` and `rten` from the binding's graph, and `cargo tree -p almiladoc` finds none of them in the default graph (cargo deny sees the binding's features in every workspace graph); a reproduced `fuzz/Cargo.lock`; `cargo about --workspace --all-features`, reaching pyo3, reproduces `THIRD-PARTY-LICENSES`; `reuse lint`; `hygiene`; online pedantic zizmor.
   - `dco`: `make dco`, which checks sign-offs, that no assistant is a commit's author or committer, and REUSE.toml's MIT-bearing annotations (ADR 0004); then `make setup`, the tracked agent setup's own check and its self-test.
   - `fuzz`: every target builds, with each declared feature; an empty seed directory fails; seeds and `fuzz/regressions/` replay once; the docs.rs build passes.

3. **Tools and caches.** `rust-toolchain.toml` pins the latest stable release; `rust-version` equals its minor. Cargo tools are sha256-pinned release binaries. Python tools, maturin and ziglang included, are locked `uv` groups. Workspace tests build at `opt-level = 1`, overflow checks and debug assertions on; `make test` also runs the recursion ceiling test at `opt-level = 0`, the dev profile the pool stack is sized for.
   - Each compiling gate job restores one entry, the Cargo registry and its own profile directories under `target/` and `fuzz/target`, keyed exactly by job, OS, architecture and a hash of its toolchain file, lockfiles, manifests, `Makefile` and `gates.yml`, without restore keys. Gate runs set `CARGO_INCREMENTAL=0` and line-tables-only debuginfo, which keeps entries small.
   - Only a `warm` run on `refs/heads/main` saves, after a miss, and only there does an exact lookup-only hit end a job early; every other run restores and runs its whole target. `.github/scripts/caches.py`, in `make actions`, holds these guards.
   - Threat model: `main` moves only by fast-forward to a green, reviewed candidate, so only reviewed code writes an entry. Pull requests, forks and candidates read `main`'s entries and save none; GitHub keeps their own scopes from `main`'s. `target/make`, holding the sha256-checked tools and the policy environment, is never cached, so no restored binary skips its checksum. Cargo rebuilds any artefact whose inputs differ from its fingerprint. zizmor's cache-poisoning audit runs in `make actions`. `release.yml` and every `setup-uv` use no cache.

4. **Schedules.** `scheduled.yml` runs weekly `cargo fuzz cmin` and daily per-target fuzz campaigns, random-seed proptests and a staleness check, against each newest stable release, of what Dependabot cannot reach: runner labels, the Makefile's tool versions and sha256s, the uv pins, `PYTHONS` and `requires-python` against supported CPython series, `deny.toml`'s advisory ignores, `about.toml`'s clarification checksums, and image digests; it alone saves the corpus cache. Public logs name only target, exit kind and input hash; failing inputs upload only encrypted, committed as reproducers after the fix's release.

5. **OCR goldens.** Deferred with ADR 0002's local OCR, a separate crate.

6. **Release.** `release.yml` runs on `v*` tag pushes, which alone publish, and rehearses on dispatch and weekly. Only publish jobs and `attest` hold `id-token: write`.
   - `guard`: advisories pass; on a tag push, the licensing FAQ exists, the annotated tag equals the workspace version, `rust-version` the toolchain minor, its commit on `main` with green `ci / required`, and `docs/release-dates.tsv` covers earlier final tags.
   - `linux`: maturin `--zig` builds `manylinux_2_28` and `musllinux_1_2` wheels on native-arch Ubuntu runners. Smoke, in PyPA's matching digest-pinned image: CPython 3.11 and 3.15 tests, a CMap-needing PDF, a page painted to a PNG of the size its dpi gives, the licence test; compressed wheels under 12 MB.
   - `native`: `macos-26`, `macos-26-intel`, `windows-2025`, `windows-11-arm`; same smoke.
   - `sdist`, `crate`: both carry `$(LICENCE_SET)`; the sdist's own wheel is smoked. `attest` covers every file.
   - `publish-testpypi` (rc tags, environment `testpypi`) and `publish-pypi` (final tags, environment `pypi`): signed attestations, trusted publishing only.
   - `publish-crates` (environment `crates-io`, final tags): `cargo publish` with an OIDC-exchanged token, revoked after use; the first final tag uses a `publish-new`-scoped environment secret, deleted after use.
   - `verify-published` checks PyPI's files and provenance publisher (workflow, environment) against attestations; crates.io's checksum against the attested `.crate` and, after the first version, its trusted-publishing repository, run and commit.
   - `github-release`: the tag message, dists and `SHA256SUMS`, uploaded to a draft that is then published, as an immutable release takes no asset after publication.

7. **Dates.** The BUSL-1.1 Change Date is the fourth anniversary of the day a version is first made publicly available: the UTC day of the earliest repository activity entry (https://docs.github.com/en/rest/repos/repos#list-repository-activities) whose `after` contains the tagged commit. The release notes, with the day its BUSL-1.1 portions become MIT, and later `docs/release-dates.tsv` record it.

8. **Supply chain.** Dependabot covers daily `cargo` (minor and patch grouped; `/fuzz` for `libfuzzer-sys` only), `uv` (minor and patch grouped), the root's `rust-toolchain` and `github-actions` (grouped), and monthly fuzz's nightly, which moves every day and only has to build cargo-fuzz. An update, Dependabot's or the maintainer's, is adopted as soon as it is released and its candidate is green, so zizmor's `dependabot-cooldown` audit is off. The maintainer adopts each update as an own signed-off commit (ADR 0004), moving fuzz's `cfb` and `zip` with the root and `rust-version` with the toolchain. `deny.toml`: permissive licences; exceptions BUSL-1.1 for `almiladoc`, the binding and fuzz crates alone, fuzz's declaring `BUSL-1.1 AND MIT`; NCSA for `libfuzzer-sys`; yanked crates, non-path wildcards, `openssl-sys`, `ort-sys` and `aws-lc-sys` denied; crates.io only; wheel targets.

9. **Settings** the maintainer checks before the first release:
   - `sha_pinning_required`; immutable releases; private vulnerability reporting; CodeQL default setup outside `required`.
   - Two `v*` tag rulesets, as a bypass spans its ruleset (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/creating-rulesets-for-a-repository.md#L23): creation, admin-bypassable only; update and deletion, unbypassable.
   - Environments `pypi`, `testpypi`, `crates-io`: maintainer-reviewed, `v*` tags only, no admin bypass.
   - Trusted publishers bind `release.yml` and the environment, optional in both registries; pending PyPI publishers precede the first rc tag.
   - Trusted publishing cannot create a crate (https://github.com/rust-lang/crates.io/blob/e15f258b77b9e73dded45339bb695960f660d33b/src/controllers/krate/publish.rs#L190-L195): the maintainer adds the trusted publisher and `trustpub_only` after decision 6's first token publish.

## Consequences

- The first candidate owes every gate input: `THIRD-PARTY-LICENSES`, with cargo-about clarifications for Adobe's BSD-3-Clause CMaps (ADR 0001 decision 6), eight fuzz seeds, a tracked `fuzz/Cargo.lock`; schedules, the recipient key and a decryption drill; the first tag, the licensing FAQ.
- New advisories red unrelated pull requests; schedules stop after 60 inactive days.
- A Dependabot `cargo` pull request moving a crate in fuzz's graph fails `locks` and `licences`: it is a notice, adopted by decision 8's own commit.
- Unexercised: zig musl builds, unguaranteed by maturin; CPython 3.11 on `windows-11-arm`, where setup-python's last arm64 build is 3.11.9 (https://github.com/actions/python-versions/blob/49858b91c92d27403416ee43117032e91d00ce24/versions-manifest.json#L10809-L10812); attestations; crates.io's exchange.

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
- `Swatinem/rust-cache`: outside the organisation's allowed actions; its prefix restore saves entries that depend on history.
- Saving from candidates, or `ci.yml` on `main`: a `land/owner/*` scope is unreadable by the next candidate, and `main` would show a second `ci / required`.
- Caching `target/make` or the rustup toolchain: a restored tool skips its checksum; the toolchain saves 10 s for about 600 MB.
