#!/bin/sh
# Run the standard validation (fmt, clippy, test) in the Linux test image, or
# the command given as arguments instead, e.g.
#   scripts/test-in-docker.sh cargo test --locked --no-run
# The checkout is mounted read-only and copied into the container, which runs
# as an ordinary user with the caller's uid. target/ and the Cargo registry
# live in named volumes; CI sets DISPATCH_DOCKER_TARGET_DIR and
# DISPATCH_DOCKER_CARGO_REGISTRY to host directories instead, for actions/cache.
# DISPATCH_DOCKER_CPUS limits the container's CPUs (docker run --cpus), to run
# the suite on a slow machine, e.g. DISPATCH_DOCKER_CPUS=1. --init gives the
# container an init that reaps orphaned processes, as a real system does;
# without it, exited orphans stay zombies and look alive to the tests.
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

cpus="${DISPATCH_DOCKER_CPUS:+--cpus=$DISPATCH_DOCKER_CPUS}"

# $cpus is empty or one word; unquoted so that empty adds no argument.
# shellcheck disable=SC2086
exec docker run --rm --init $cpus \
  -v "$PWD:/src:ro" -v "$target:/target" -v "$registry:/usr/local/cargo/registry" \
  -e CARGO_TARGET_DIR=/target \
  dispatch-test sh -c 'tar -C /src --exclude=./target -cf - . | tar -C /work -xf - && exec "$@"' sh "$@"
