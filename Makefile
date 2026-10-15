SHELL := bash
.SHELLFLAGS := -euo pipefail -c
.ONESHELL:
.DELETE_ON_ERROR:
MAKEFLAGS += --no-builtin-rules

CARGO ?= cargo
UV_VERSION := 0.13.1
UV := uv tool run uv@$(UV_VERSION)
PYTHONS ?= 3.11 3.15
LICENCE_SET := LICENSE LICENSES/ NOTICE REUSE.toml
LICENCE_EXPRESSION := FSL-1.1-MIT AND MIT
OUT := $(abspath $(or $(CARGO_TARGET_DIR),target))/make

export RUSTFLAGS := -D warnings
export RUSTDOCFLAGS := -D warnings
export INSTA_UPDATE := no
export INSTA_FORCE_PASS := 0
unexport INSTA_FORCE_UPDATE INSTA_FORCE_UPDATE_SNAPSHOTS
export PROPTEST_RNG_SEED := 20261015

not_run = echo "NOT RUN $@: $(1)"; [ "$${CI:-}" != true ] || exit 1

.PHONY: ci fmt clippy test doc features python

ci: fmt clippy test doc features python

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
