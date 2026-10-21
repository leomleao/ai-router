# syntax=docker/dockerfile:1
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl bash \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 router \
    && useradd --uid 10001 --gid 10001 --home-dir /var/lib/ai-router/antigravity-auth --no-create-home --shell /bin/bash router \
    && mkdir -p /var/lib/ai-router/antigravity-auth /var/lib/ai-router/openai-auth /var/lib/ai-router/telemetry /run/ai-router \
    && chown -R router:router /var/lib/ai-router /run/ai-router \
    && chmod 0700 /var/lib/ai-router/antigravity-auth /var/lib/ai-router/openai-auth /var/lib/ai-router/telemetry /run/ai-router
ENV HOME=/var/lib/ai-router/antigravity-auth \
    AI_ROUTER_STATE_DIR=/var/lib/ai-router/antigravity-auth \
    AI_ROUTER_ANTIGRAVITY_ENABLED=true \
    AI_ROUTER_OPENAI_ENABLED=false \
    AI_ROUTER_OPENAI_STATE_DIR=/var/lib/ai-router/openai-auth \
    AI_ROUTER_OPENAI_MAX_CONCURRENT=2 \
    AI_ROUTER_OPENAI_MAX_QUEUE=8 \
    AI_ROUTER_TELEMETRY_DIR=/var/lib/ai-router/telemetry \
    AI_ROUTER_MONITOR_SOCKET=/run/ai-router/monitor.sock \
    TMPDIR=/tmp
USER 10001:10001
WORKDIR /

FROM rust:1.91-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY docs/antigravity-baseline.json ./docs/antigravity-baseline.json
RUN cargo build --locked --release

# n8n's pinned Vercel SDK contract runs only in the isolated test image.
FROM node:24.10.0-bookworm-slim AS n8n-contract-deps
WORKDIR /opt/n8n-contract
RUN npm install --ignore-scripts --no-audit --no-fund --save-exact \
    ai@7.0.66 @ai-sdk/openai@4.0.20 zod@4.1.8 \
    @langchain/openai@1.4.4 @langchain/core@1.2.8
COPY tests/n8n_contract.mjs ./n8n_contract.mjs

# Supabase Studio's pinned assistant SDKs, isolated from n8n's versions.
FROM node:24.10.0-bookworm-slim AS studio-contract-deps
WORKDIR /opt/studio-contract
RUN npm install --ignore-scripts --no-audit --no-fund --save-exact \
    ai@7.0.59 @ai-sdk/openai@4.0.41 zod@4.1.8

# Local deterministic tests only. The production image contains neither Python nor Node.
FROM builder AS test
USER root
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv libatomic1 \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/test-venv \
    && /opt/test-venv/bin/pip install --no-cache-dir openai==3.24.0 pyte==0.8.2 \
    && mkdir -p /var/lib/ai-router/antigravity-auth /var/lib/ai-router/openai-auth /var/lib/ai-router/telemetry /run/ai-router \
    && chown -R 10001:10001 /var/lib/ai-router /run/ai-router \
    && chmod 0700 /var/lib/ai-router/antigravity-auth /var/lib/ai-router/openai-auth /var/lib/ai-router/telemetry /run/ai-router
ENV PATH=/opt/test-venv/bin:$PATH
COPY --from=n8n-contract-deps /usr/local/bin/node /usr/local/bin/node
COPY --from=n8n-contract-deps /opt/n8n-contract /opt/n8n-contract
COPY --from=studio-contract-deps /opt/studio-contract /opt/studio-contract
COPY tests ./tests
COPY scripts/login.sh scripts/antigravity.sh scripts/openai.sh ./scripts/
# The PTY and login checks run the debug binary, which cargo test does not build.
RUN cargo test --locked && cargo build --locked && python3 tests/monitor_tui.py && python3 tests/login_contract.py

FROM runtime AS gateway
COPY THIRD_PARTY_NOTICES.md /usr/share/doc/ai-router/THIRD_PARTY_NOTICES.md
COPY docs/antigravity-baseline.json /usr/share/doc/ai-router/antigravity-baseline.json
COPY --from=builder /build/target/release/ai-router /usr/local/bin/ai-router
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD curl --fail --silent http://127.0.0.1:8080/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/ai-router"]
CMD ["serve"]
