#!/bin/sh
# Run the standard validation in the Linux test image. The checkout is mounted
# read-only and copied into the container; target/ lives in a named volume.
# P0 skeleton; P5 completes it.
set -eu
cd "$(dirname "$0")/.."
docker build -q -t dispatch-test -f ci/test.Dockerfile ci >/dev/null
exec docker run --rm \
  -v "$PWD:/src:ro" -v dispatch-test-target:/target -v dispatch-test-cargo:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target \
  dispatch-test sh -c 'tar -C /src --exclude=./target -cf - . | tar -C /work -xf - && cd /work && cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked'
