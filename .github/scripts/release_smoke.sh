#!/usr/bin/env bash
# release_smoke.sh WHEEL SDIST REQUIREMENTS, from the repository root: the wheel's size, its
# licence set beside the sdist's, and the binding's tests under each CPython in PYTHONS, taken
# from a PyPA image's /opt/python or else from uv.
set -euo pipefail

wheel=$1 sdist=$2 requirements=$3
max_bytes=12000000
size=$(wc -c < "$wheel")
if [ "$size" -ge "$max_bytes" ]; then
  echo "smoke: $wheel is $size bytes, not under $max_bytes"
  exit 1
fi

read -ra pythons <<< "$(sed -n 's/^PYTHONS ?= //p' Makefile)"
read -ra licence_set <<< "$(sed -n 's/^LICENCE_SET := //p' Makefile)"
expression=$(sed -n 's/^LICENCE_EXPRESSION := //p' Makefile)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for version in "${pythons[@]}"; do
  tag=cp${version/./}
  python=/opt/python/$tag-$tag/bin/python3
  if [ ! -x "$python" ]; then python=$(uv python find --no-project "$version"); fi
  venv=$work/venv-$version
  "$python" -I -m venv "$venv"
  bin=$venv/bin
  if [ -d "$venv/Scripts" ]; then bin=$venv/Scripts; fi
  "$bin/python" -I -m pip install --quiet --disable-pip-version-check --no-deps --require-hashes -r "$requirements"
  "$bin/python" -I -m pip install --quiet --disable-pip-version-check --no-deps "$wheel"
  "$bin/python" -I -B -c 'import almiladoc, sys; assert sys.version.startswith(sys.argv[1] + "."), sys.version' "$version"
  "$bin/python" -I -B -m pytest -W error -p no:cacheprovider python/tests
done

"$bin/python" -I -B scripts/check_dist.py --expression "$expression" --licence-set "${licence_set[@]}" \
  --wheel "$wheel" --sdist "$sdist"
