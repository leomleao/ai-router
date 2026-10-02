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

# Local deterministic tests only. Python executes the synthetic fake provider;
# the production gateway stage contains no Python runtime.
FROM builder AS test
USER root
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/test-venv \
    && /opt/test-venv/bin/pip install --no-cache-dir openai==3.24.0 \
    && mkdir -p /var/lib/ai-router/auth /var/lib/ai-router/telemetry /run/ai-router \
    && chown -R 10001:10001 /var/lib/ai-router /run/ai-router \
    && chmod 0700 /var/lib/ai-router/auth /var/lib/ai-router/telemetry /run/ai-router
ENV PATH=/opt/test-venv/bin:$PATH
COPY tests ./tests
RUN cargo test --locked

FROM agy-runtime AS gateway
COPY --from=builder /build/target/release/ai-router /usr/local/bin/ai-router
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD curl --fail --silent http://127.0.0.1:8080/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/ai-router"]
CMD ["serve"]
