# The Linux test image: what the standard validation (fmt, clippy, test) needs
# and nothing else. The same image runs locally and in CI, always through
# scripts/test-in-docker.sh. Every version is pinned: the base image by digest
# (rust 1.97.1 on Debian bookworm) and apt packages by version.
#
# Tests run as an ordinary user: root ignores the permission denials that
# several tests rely on. scripts/test-in-docker.sh builds the image with the
# caller's uid, so host directories it mounts stay writable.
FROM rust:1.97.1-slim-bookworm@sha256:2775a09d208ff0d7c1f50490c45b62db929e87ba1dcbc3f2132ac71a704bcdd3

ARG TEST_UID=1000

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      curl=7.88.1-10+deb12u15 \
      git=1:2.39.5-0+deb12u3 \
      procps=2:4.0.2-3 \
      python3=3.11.2-1+b1 \
      sqlite3=3.40.1-2+deb12u2 \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy \
 && git config --system user.name "Dispatch Tests" \
 && git config --system user.email "tests@dispatch.invalid" \
 && git config --system init.defaultBranch main \
 && useradd --create-home --uid "$TEST_UID" --shell /bin/bash dispatch \
 && mkdir -p /work /target /usr/local/cargo/registry \
 && chown -R dispatch:dispatch /work /target /usr/local/cargo

USER dispatch
WORKDIR /work
