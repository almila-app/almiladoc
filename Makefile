SHELL := bash
.SHELLFLAGS := -euo pipefail -c
.ONESHELL:
.DELETE_ON_ERROR:
MAKEFLAGS += --no-builtin-rules

CARGO ?= cargo
UV_VERSION := 0.13.1
UV := uv tool run uv@$(UV_VERSION)
PYTHONS ?= 3.11 3.15
LICENCE_SET := LICENSE LICENSES/ NOTICE REUSE.toml THIRD-PARTY-LICENSES
LICENCE_EXPRESSION := FSL-1.1-MIT AND MIT
OUT := $(abspath $(or $(CARGO_TARGET_DIR),target))/make
TOOLS := $(OUT)/tools
POLICY := $(OUT)/policy
POLICY_PYTHON := $(POLICY)/bin/python -I -B
HOST_ARCH := $(if $(filter Linux,$(shell uname -s)),$(shell uname -m))

CARGO_DENY_VERSION := 0.20.2
CARGO_DENY_SHA256_x86_64 := 9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f
CARGO_DENY_SHA256_aarch64 := 995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d
CARGO_ABOUT_VERSION := 0.9.2
CARGO_ABOUT_SHA256_x86_64 := 9099a59e820c38a68b9d65f300662a567d56562f9a10f6aa4c7e86c17c2566af
CARGO_ABOUT_SHA256_aarch64 := af5169282fb6f84e13471493f405437e43ac517744c9ae12fbe2cdf0a6f0e5a8
CARGO_FUZZ_VERSION := 0.13.2
CARGO_FUZZ_SHA256_x86_64 := b5b704018b63e0f151c17a057ac53b5111e1db545d1b9f72fee79f08a545931c
CARGO_DENY := $(TOOLS)/cargo-deny-$(CARGO_DENY_VERSION)/cargo-deny
CARGO_ABOUT := $(TOOLS)/cargo-about-$(CARGO_ABOUT_VERSION)/cargo-about
CARGO_FUZZ := $(TOOLS)/cargo-fuzz-$(CARGO_FUZZ_VERSION)/cargo-fuzz
MUSL := $(HOST_ARCH)-unknown-linux-musl

export RUSTFLAGS := -D warnings
export RUSTDOCFLAGS := -D warnings
export INSTA_UPDATE := no
export INSTA_FORCE_PASS := 0
unexport INSTA_FORCE_UPDATE INSTA_FORCE_UPDATE_SNAPSHOTS
export PROPTEST_RNG_SEED := 20261015

not_run = echo "NOT RUN $@: $(1)"; [ "$${CI:-}" != true ] || exit 1
need_tool = if [ ! -x $(1) ]; then $(call not_run,no pinned $(notdir $(1)) binary for $$(uname -sm)); exit 0; fi
refresh_index = $(CARGO) update --quiet --dry-run --manifest-path $(1)
need_uv = if ! command -v $(firstword $(UV)) >/dev/null; then $(call not_run,uv is not installed); exit 0; fi
policy_sync = UV_PROJECT_ENVIRONMENT=$(POLICY) $(UV) sync --locked --only-group policy --quiet
# reuse ships only an sdist for these Pythons, so it builds against the group's locked poetry-core.
policy_env = $(policy_sync) --no-install-package reuse && $(policy_sync) --no-build-isolation-package reuse

# fetch(url, sha256, member): an archive's binary, installed only once its sha256 matches.
define fetch
if [ -z "$(2)" ]; then echo "$(notdir $@): no pinned binary for $$(uname -sm)"; exit 0; fi
rm -rf $@.download
mkdir -p $@.download
curl -fsSL --proto '=https' --tlsv1.2 -A almiladoc-ci -o $@.download/archive.tar.gz '$(1)'
echo '$(2)  $@.download/archive.tar.gz' | sha256sum -c --quiet
tar -xzf $@.download/archive.tar.gz -C $@.download
install -m 0755 $@.download/$(3) $@
rm -rf $@.download
endef

.PHONY: ci fmt clippy test doc features python tools policy locks licences reuse hygiene actions dco provenance fuzz

ci: fmt clippy test doc features python policy locks licences reuse hygiene actions dco provenance fuzz

fmt:
	$(CARGO) fmt --all --check
	rustfmt --check --edition 2024 fuzz/fuzz_targets/*.rs

clippy:
	$(CARGO) clippy --locked --workspace --all-targets --all-features

test:
	$(CARGO) test --locked --all-features
	pending=$$(find . -path ./target -prune -o \( -name '*.snap.new' -o -name '*.pending-snap' \) -print)
	if [ -n "$$pending" ]; then echo "unreviewed snapshots:"; echo "$$pending"; exit 1; fi

doc:
	$(CARGO) doc --locked --workspace --all-features --no-deps

features:
	$(CARGO) clippy --locked --workspace --all-targets --no-default-features
	$(CARGO) clippy --locked -p almiladoc --all-targets --no-default-features
	$(CARGO) test --locked --no-default-features
	features=$$($(CARGO) metadata --locked --no-deps --format-version 1 \
	  | python3 -I -c 'import json, sys; [print(p["name"], f) for p in json.load(sys.stdin)["packages"] for f in sorted(p["features"]) if f != "default"]')
	if [ -z "$$features" ]; then echo "features: no crate declares a feature, so the per-feature check is vacuous"; fi
	while read -r package feature; do
	  [ -n "$$package" ] || continue
	  $(CARGO) clippy --locked -p "$$package" --all-targets --no-default-features --features "$$feature"
	  if [ "$$package" = almiladoc ]; then $(CARGO) test --locked --no-default-features --features "$$feature"; fi
	done <<< "$$features"

python:
	if ! command -v $(firstword $(UV)) >/dev/null; then $(call not_run,uv is not installed); exit 0; fi
	rm -rf $(OUT)/python
	mkdir -p $(OUT)/python/dist
	UV_PROJECT_ENVIRONMENT=$(OUT)/python/build $(UV) sync --locked --only-group build
	$(OUT)/python/build/bin/maturin sdist --out $(OUT)/python/dist
	tar -xzmf $(OUT)/python/dist/*.tar.gz -C $(OUT)/python
	(cd $(OUT)/python/almiladoc-*/ && $(OUT)/python/build/bin/maturin build --release --locked --out $(OUT)/python/dist)
	$(OUT)/python/build/bin/python -I -B scripts/check_dist.py --expression '$(LICENCE_EXPRESSION)' --licence-set $(LICENCE_SET) \
	  --wheel $(OUT)/python/dist/*.whl --sdist $(OUT)/python/dist/*.tar.gz
	for version in $(PYTHONS); do
	  venv=$(OUT)/python/venv-$$version
	  UV_PROJECT_ENVIRONMENT=$$venv $(UV) sync --locked --only-group test --python $$version
	  $(UV) pip install --python $$venv/bin/python --no-deps $(OUT)/python/dist/*.whl
	  $$venv/bin/python -I -B -c 'import almiladoc, sys, sysconfig; from pathlib import Path; assert sys.version.startswith(sys.argv[1] + "."), sys.version; assert Path(almiladoc.__file__).is_relative_to(sysconfig.get_path("purelib")), almiladoc.__file__' $$version
	  $$venv/bin/python -I -B -m pytest -W error -p no:cacheprovider python/tests
	  (cd $$venv && bin/python -I -B -m mypy.stubtest --allowlist $(CURDIR)/python/stubtest-allowlist.txt almiladoc)
	done

tools: $(CARGO_DENY) $(CARGO_ABOUT) $(CARGO_FUZZ)

$(CARGO_DENY):
	$(call fetch,https://github.com/EmbarkStudios/cargo-deny/releases/download/$(CARGO_DENY_VERSION)/cargo-deny-$(CARGO_DENY_VERSION)-$(MUSL).tar.gz,$(CARGO_DENY_SHA256_$(HOST_ARCH)),cargo-deny-$(CARGO_DENY_VERSION)-$(MUSL)/cargo-deny)

$(CARGO_ABOUT):
	$(call fetch,https://github.com/EmbarkStudios/cargo-about/releases/download/$(CARGO_ABOUT_VERSION)/cargo-about-$(CARGO_ABOUT_VERSION)-$(MUSL).tar.gz,$(CARGO_ABOUT_SHA256_$(HOST_ARCH)),cargo-about-$(CARGO_ABOUT_VERSION)-$(MUSL)/cargo-about)

$(CARGO_FUZZ):
	$(call fetch,https://github.com/rust-fuzz/cargo-fuzz/releases/download/$(CARGO_FUZZ_VERSION)/cargo-fuzz-$(CARGO_FUZZ_VERSION)-$(MUSL).tar.gz,$(CARGO_FUZZ_SHA256_$(HOST_ARCH)),cargo-fuzz)

policy: $(CARGO_DENY)
	$(call need_tool,$(CARGO_DENY))
	$(call refresh_index,Cargo.toml)
	$(CARGO_DENY) --workspace --all-features --locked --config deny.toml check
	$(call refresh_index,fuzz/Cargo.toml)
	$(CARGO_DENY) --manifest-path fuzz/Cargo.toml --all-features --locked --config deny.toml check
	$(CARGO_DENY) --locked --config deny-offline.toml check bans

locks:
	$(call need_uv)
	$(policy_env)
	$(POLICY_PYTHON) .github/scripts/locks.py --cargo $(CARGO) --self-test
	$(POLICY_PYTHON) .github/scripts/locks.py --cargo $(CARGO)

licences: $(CARGO_ABOUT)
	$(call need_tool,$(CARGO_ABOUT))
	mkdir -p $(OUT)
	$(CARGO_ABOUT) generate --locked --workspace --all-features --fail --config about.toml --output-file $(OUT)/THIRD-PARTY-LICENSES about.hbs
	if ! cmp -s $(OUT)/THIRD-PARTY-LICENSES THIRD-PARTY-LICENSES; then
	  cp $(OUT)/THIRD-PARTY-LICENSES THIRD-PARTY-LICENSES
	  echo "THIRD-PARTY-LICENSES was stale and is regenerated: review and commit it"
	  exit 1
	fi

reuse:
	$(call need_uv)
	$(policy_env)
	$(POLICY)/bin/reuse lint

hygiene:
	$(call need_uv)
	$(policy_env)
	$(POLICY_PYTHON) .github/scripts/hygiene.py --self-test
	$(POLICY_PYTHON) .github/scripts/staleness.py --self-test
	$(POLICY_PYTHON) .github/scripts/hygiene.py

actions:
	$(call need_uv)
	$(policy_env)
	if [ -n "$${GH_TOKEN:-}" ]; then
	  $(POLICY)/bin/zizmor --pedantic .
	elif [ "$${CI:-}" = true ]; then
	  echo "actions: GH_TOKEN is unset, so impostor commits go unchecked"; exit 1
	else
	  echo "actions: offline, since GH_TOKEN is unset"
	  $(POLICY)/bin/zizmor --pedantic --offline .
	fi

dco:
	$(call need_uv)
	$(policy_env)
	$(POLICY_PYTHON) .github/scripts/dco.py --self-test
	if [ -n "$${DCO_BASE:-}" ]; then
	  $(POLICY_PYTHON) .github/scripts/dco.py --pull-request --base "$$DCO_BASE" --head "$${DCO_HEAD:?DCO_HEAD names the pull request head}"
	else
	  $(POLICY_PYTHON) .github/scripts/dco.py --base origin/main --head HEAD
	fi

provenance:
	$(call need_uv)
	$(policy_env)
	$(POLICY_PYTHON) .github/scripts/provenance.py --self-test
	$(POLICY_PYTHON) .github/scripts/provenance.py

# cargo-fuzz's release binary is a musl build, which defaults --target to its own triple.
fuzz: $(CARGO_FUZZ)
	$(call need_tool,$(CARGO_FUZZ))
	export PATH="$(dir $(CARGO_FUZZ)):$$PATH"
	cd fuzz
	cargo metadata --locked --format-version 1 >/dev/null
	host=$$(rustc -vV | sed -n 's/^host: //p')
	targets=$$(cargo fuzz list)
	for target in $$targets; do
	  if [ -z "$$(find seeds/$$target -type f ! -name '.*' -print -quit 2>/dev/null)" ]; then echo "fuzz: seeds/$$target is missing or empty"; exit 1; fi
	  if [ ! -d regressions/$$target ]; then echo "fuzz: regressions/$$target is missing"; exit 1; fi
	done
	features=$$(cargo metadata --locked --no-deps --format-version 1 \
	  | python3 -I -c 'import json, sys; [print(f) for p in json.load(sys.stdin)["packages"] for f in sorted(p["features"]) if f != "default"]')
	cargo fuzz build --target "$$host"
	for feature in $$features; do cargo fuzz build --target "$$host" --features "$$feature"; done
	for target in $$targets; do
	  cargo fuzz run --target "$$host" $$target seeds/$$target regressions/$$target -- -runs=0
	done
	RUSTDOCFLAGS="$$RUSTDOCFLAGS --cfg docsrs" cargo doc --locked --manifest-path ../Cargo.toml --no-deps
