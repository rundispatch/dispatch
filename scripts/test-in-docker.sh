#!/bin/sh
# Run the standard validation (fmt, clippy, test) in the Linux test image, or
# the command given as arguments instead, e.g.
#   scripts/test-in-docker.sh cargo test --locked --no-run
# The checkout is mounted read-only and copied into the container, which runs
# as an ordinary user with the caller's uid. target/ and the Cargo registry
# live in named volumes; CI sets DISPATCH_DOCKER_TARGET_DIR and
# DISPATCH_DOCKER_CARGO_REGISTRY to host directories instead, for actions/cache.
set -eu
cd "$(dirname "$0")/.."

uid="$(id -u)"
# useradd refuses uid 0; a root caller gets the image's default user.
[ "$uid" -ne 0 ] || uid=1000
docker build -q -t dispatch-test --build-arg TEST_UID="$uid" -f ci/test.Dockerfile ci >/dev/null

target="${DISPATCH_DOCKER_TARGET_DIR:-dispatch-test-target-$uid}"
registry="${DISPATCH_DOCKER_CARGO_REGISTRY:-dispatch-test-cargo-$uid}"
if [ "$#" -eq 0 ]; then
  set -- sh -c 'cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked'
fi

exec docker run --rm \
  -v "$PWD:/src:ro" -v "$target:/target" -v "$registry:/usr/local/cargo/registry" \
  -e CARGO_TARGET_DIR=/target \
  dispatch-test sh -c 'tar -C /src --exclude=./target -cf - . | tar -C /work -xf - && exec "$@"' sh "$@"
