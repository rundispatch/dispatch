# The Linux test image: what `cargo test` needs and nothing else. The same
# image runs locally (scripts/test-in-docker.sh) and in CI. P0 skeleton; P5
# pins and completes it.
FROM rust:1.97-slim-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends git python3 procps ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy \
 && git config --system user.name "Dispatch Tests" \
 && git config --system user.email "tests@dispatch.invalid" \
 && git config --system init.defaultBranch main
WORKDIR /work
