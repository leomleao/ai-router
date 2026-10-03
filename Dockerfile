# syntax=docker/dockerfile:1
FROM debian:bookworm-slim AS agy-runtime
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl bash libstdc++6 libsecret-1-0 bubblewrap util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 router \
    && useradd --uid 10001 --gid 10001 --home-dir /var/lib/ai-router/auth --no-create-home --shell /bin/bash router \
    && mkdir -p /var/lib/ai-router/auth /var/lib/ai-router/telemetry /run/ai-router/workspaces \
    && chown -R router:router /var/lib/ai-router /run/ai-router \
    && chmod 0700 /var/lib/ai-router/auth /var/lib/ai-router/telemetry /run/ai-router
COPY scripts/install-agy.sh /tmp/install-agy.sh
RUN sh /tmp/install-agy.sh "$TARGETARCH" && rm /tmp/install-agy.sh
ENV HOME=/var/lib/ai-router/auth \
    AGY_CLI_DISABLE_AUTO_UPDATE=true \
    AI_ROUTER_AGY_BIN=/usr/local/bin/agy \
    AI_ROUTER_STATE_DIR=/var/lib/ai-router/auth \
    AI_ROUTER_WORKSPACE_DIR=/run/ai-router/workspaces \
    AI_ROUTER_TELEMETRY_DIR=/var/lib/ai-router/telemetry \
    AI_ROUTER_MONITOR_SOCKET=/run/ai-router/monitor.sock \
    TMPDIR=/run/ai-router/workspaces
USER 10001:10001
# Do not put the image cwd under a tmpfs mount: Docker would recreate that
# directory as root, preventing the nonroot runner from creating workspaces.
WORKDIR /
# This intermediate target permits interactive login before the gateway build.
CMD ["sleep", "infinity"]

FROM rust:1.91-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

# n8n's pinned Vercel SDK contract runs only in the isolated test image.
FROM node:24.10.0-bookworm-slim AS n8n-contract-deps
WORKDIR /opt/n8n-contract
RUN npm install --ignore-scripts --no-audit --no-fund --save-exact \
    ai@7.0.66 @ai-sdk/openai@4.0.20 zod@4.1.8
COPY tests/n8n_contract.mjs ./n8n_contract.mjs

# Local deterministic tests only. Python executes the synthetic fake provider;
# the production gateway stage contains neither Python nor Node.
FROM builder AS test
USER root
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv libatomic1 \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/test-venv \
    && /opt/test-venv/bin/pip install --no-cache-dir openai==3.24.0 pyte==0.8.2 \
    && mkdir -p /var/lib/ai-router/auth /var/lib/ai-router/telemetry /run/ai-router \
    && chown -R 10001:10001 /var/lib/ai-router /run/ai-router \
    && chmod 0700 /var/lib/ai-router/auth /var/lib/ai-router/telemetry /run/ai-router
ENV PATH=/opt/test-venv/bin:$PATH
COPY --from=n8n-contract-deps /usr/local/bin/node /usr/local/bin/node
COPY --from=n8n-contract-deps /opt/n8n-contract /opt/n8n-contract
COPY tests ./tests
RUN cargo test --locked && python3 tests/monitor_tui.py

FROM agy-runtime AS gateway
COPY --from=builder /build/target/release/ai-router /usr/local/bin/ai-router
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD curl --fail --silent http://127.0.0.1:8080/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/ai-router"]
CMD ["serve"]
