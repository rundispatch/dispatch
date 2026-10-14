# The Linux test image: what the standard validation (fmt, clippy, test) needs
# and nothing else. The same image runs locally and in CI, always through
# scripts/test-in-docker.sh. Every version is pinned: the base image by digest
# (rust 1.97.1 on Debian bookworm), apt packages by version, and the Docker CLI
# by version and checksum.
#
# The Docker CLI (client only, no daemon) is here because the executor's Docker
# backend resolves `docker` from trusted system directories, and its unit test
# checks that resolution.
#
# Tests run as an ordinary user: root ignores the permission denials that
# several tests rely on. scripts/test-in-docker.sh builds the image with the
# caller's uid, so host directories it mounts stay writable.
FROM rust:1.97.1-slim-bookworm@sha256:2775a09d208ff0d7c1f50490c45b62db929e87ba1dcbc3f2132ac71a704bcdd3

ARG DOCKER_CLI_VERSION=29.6.1
ARG DOCKER_CLI_SHA256_X86_64=b0df4a43a98d7ecb708acbdb5a34a3416e13b6e39bcbbdf296f51f0f3442b29f
ARG DOCKER_CLI_SHA256_AARCH64=917a4bb83565bcacb38c430f08daae8b59db3256331ac23f22394f0542509881
ARG TEST_UID=1000

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      curl=7.88.1-10+deb12u15 \
      git=1:2.39.5-0+deb12u3 \
      procps=2:4.0.2-3 \
      python3=3.11.2-1+b1 \
      sqlite3=3.40.1-2+deb12u2 \
 && rm -rf /var/lib/apt/lists/* \
 && arch="$(uname -m)" \
 && case "$arch" in \
      x86_64) sum="$DOCKER_CLI_SHA256_X86_64" ;; \
      aarch64) sum="$DOCKER_CLI_SHA256_AARCH64" ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/docker.tgz \
      "https://download.docker.com/linux/static/stable/${arch}/docker-${DOCKER_CLI_VERSION}.tgz" \
 && echo "$sum  /tmp/docker.tgz" | sha256sum -c - \
 && tar -xzf /tmp/docker.tgz -C /usr/local/bin --strip-components=1 --no-same-owner docker/docker \
 && rm /tmp/docker.tgz \
 && docker --version \
 && rustup component add rustfmt clippy \
 && git config --system user.name "Dispatch Tests" \
 && git config --system user.email "tests@dispatch.invalid" \
 && git config --system init.defaultBranch main \
 && useradd --create-home --uid "$TEST_UID" --shell /bin/bash dispatch \
 && mkdir -p /work /target /usr/local/cargo/registry \
 && chown -R dispatch:dispatch /work /target /usr/local/cargo

USER dispatch
WORKDIR /work
