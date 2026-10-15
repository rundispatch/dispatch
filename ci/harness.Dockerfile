# The harness trial image: the test image plus pinned agent CLIs, for
# scripts/harness-trial.sh. Build the test image first, through
# scripts/test-in-docker.sh (it tags dispatch-test with the caller's uid), then:
#   docker build -t dispatch-harness -f ci/harness.Dockerfile ci
#
# No credentials are in the image; trials pass them in the environment. It runs
# as the test image's ordinary user. Self-update is disabled where a CLI has it:
# Claude Code by DISABLE_AUTOUPDATER, Cursor by its "static" update channel.
# Codex and Hermes only print a notice that an update exists.
FROM dispatch-test

ARG NODE_VERSION=22.20.0
ARG NODE_SHA256_X64=00bbd05e306ea68b6e13e17360d0e2f680b493ef95f2fea1c4296ff7437530bc
ARG NODE_SHA256_ARM64=06907b9c088ce62305bc1530e5c1ae1510245114645768f7750c349c5b6fe667
ARG CLAUDE_CODE_VERSION=2.1.296
ARG CODEX_VERSION=0.155.1
ARG CURSOR_AGENT_VERSION=2026.10.14-0f9edde
ARG CURSOR_AGENT_SHA256_X64=74fac931fe3578980d070701872b7dab54eb87a9abf9b6e2e6be19beeb9167a7
ARG CURSOR_AGENT_SHA256_ARM64=28028e8adb8266c8b7c00d11950765fbf9a3b9cac77a98b7c92b172646b35f4b
ARG HERMES_AGENT_VERSION=0.18.2

USER root
# Node, then Claude Code and Codex from npm, as both vendors document.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      python3-venv=3.11.2-1+b1 \
      xz-utils=5.4.1-1+deb12u2 \
 && rm -rf /var/lib/apt/lists/* \
 && case "$(uname -m)" in \
      x86_64) arch=x64; sum="$NODE_SHA256_X64" ;; \
      aarch64) arch=arm64; sum="$NODE_SHA256_ARM64" ;; \
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/node.tar.xz \
      "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${arch}.tar.xz" \
 && echo "$sum  /tmp/node.tar.xz" | sha256sum -c - \
 && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
      --exclude='*.md' --exclude=LICENSE \
 && rm /tmp/node.tar.xz \
 && npm install -g --no-fund --no-audit \
      "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
      "@openai/codex@${CODEX_VERSION}" \
 && npm cache clean --force
# Hermes Agent, the PyPI release in its own virtual environment. Its install
# script follows a Git branch, so it cannot pin a release.
RUN python3 -m venv /opt/hermes \
 && /opt/hermes/bin/pip install --no-cache-dir "hermes-agent==${HERMES_AGENT_VERSION}" \
 && ln -s /opt/hermes/bin/hermes /usr/local/bin/hermes

USER dispatch
# Cursor Agent: the archive and layout of its installer (cursor.com/install),
# at a pinned version instead of the latest.
RUN case "$(uname -m)" in \
      x86_64) arch=x64; sum="$CURSOR_AGENT_SHA256_X64" ;; \
      aarch64) arch=arm64; sum="$CURSOR_AGENT_SHA256_ARM64" ;; \
    esac \
 && versions="$HOME/.local/share/cursor-agent/versions" \
 && mkdir -p "$versions/$CURSOR_AGENT_VERSION" "$HOME/.local/bin" "$HOME/.cursor" \
 && curl -fsSL -o /tmp/cursor-agent.tar.gz \
      "https://downloads.cursor.com/lab/${CURSOR_AGENT_VERSION}/linux/${arch}/agent-cli-package.tar.gz" \
 && echo "$sum  /tmp/cursor-agent.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/cursor-agent.tar.gz -C "$versions/$CURSOR_AGENT_VERSION" --strip-components=1 \
 && rm /tmp/cursor-agent.tar.gz \
 && ln -s "$versions/$CURSOR_AGENT_VERSION/cursor-agent" "$HOME/.local/bin/agent" \
 && ln -s "$versions/$CURSOR_AGENT_VERSION/cursor-agent" "$HOME/.local/bin/cursor-agent" \
 && echo '{"version":1,"channel":"static"}' > "$HOME/.cursor/cli-config.json"

ENV PATH=/home/dispatch/.local/bin:$PATH \
    DISABLE_AUTOUPDATER=1
WORKDIR /home/dispatch

RUN claude --version && codex --version && cursor-agent --version && hermes --version \
 && cursor-agent update | grep -Fx "Skipping update check for static channel" \
 && rm -rf "$HOME/.cache" "$HOME/.codex" "$HOME/.hermes"
