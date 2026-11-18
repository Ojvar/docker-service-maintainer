# service-maintainer: multi-stage build.
#   1. deps    - compile only the dependencies (cached until Cargo.toml / Cargo.lock change)
#   2. builder - compile the app (templates/ are embedded with include_str!)
#   3. runtime - debian:bookworm-slim (same glibc as the builder) + the tools the scripts call
#
# scripts/ is baked into the image at /app/scripts (SCRIPTS_DIR): script changes need a new image.
# Persistent server-monitoring data lives in /app/data (SERVERS_DATA_DIR): mount it.
#
# Base images are overridable (docker.io is reached through the daemon's registry mirror):
#   docker build --build-arg RUNTIME_IMAGE=docker.qeng.ir/debian:bookworm-slim ...

ARG RUST_IMAGE=docker.qeng.ir/rust:1-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim
ARG DOCKER_CLI_IMAGE=docker:25-cli

# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS deps
WORKDIR /build
ENV CARGO_TERM_COLOR=always
COPY Cargo.toml Cargo.lock ./
# Build the dependency graph with a stub main so this layer is reused when only src/ changes.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/service-maintainer* target/release/deps/service_maintainer-* \
           target/release/.fingerprint/service-maintainer-*

# ---------------------------------------------------------------------------
FROM deps AS builder
COPY src ./src
COPY templates ./templates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    touch src/main.rs \
 && cargo build --release --locked \
 && strip target/release/service-maintainer \
 && ./target/release/service-maintainer --version

# ---------------------------------------------------------------------------
FROM ${DOCKER_CLI_IMAGE} AS dockercli

# ---------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime

# UID/GID of the host user that owns the mounted scripts/, data/ and ssh key (qeng = 1000).
ARG APP_USER=qeng
ARG APP_UID=1000
ARG APP_GID=1000

# Runtime tools used by the mounted scripts:
#   bash, coreutils (timeout/date/mktemp/seq) - base image
#   openssh-client - list/restore backups, server monitoring (ssh, ssh-keyscan, ssh-keygen)
#   curl           - alert webhook (servers/send-alert.sh) and the HEALTHCHECK
#   ca-certificates- https webhooks
#   tini           - PID 1: forwards SIGTERM to the app and reaps orphaned script children
#   docker CLI     - docker service/stack commands against the mounted /var/run/docker.sock
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        bash ca-certificates curl openssh-client tini tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid "${APP_GID}" "${APP_USER}" \
 && useradd --uid "${APP_UID}" --gid "${APP_GID}" --home-dir "/home/${APP_USER}" \
        --create-home --shell /bin/bash "${APP_USER}" \
 && install -d -m 700 -o "${APP_UID}" -g "${APP_GID}" "/home/${APP_USER}/.ssh" "/home/${APP_USER}/.docker" \
 && install -d -m 755 -o "${APP_UID}" -g "${APP_GID}" /app /app/data

COPY --from=dockercli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=builder /build/target/release/service-maintainer /usr/local/bin/service-maintainer

# Bundled scripts: owned by root (the app user can run but not modify them), LF line endings,
# *.sh executable, everything else (servers/servers.conf) read-only.
COPY --chown=0:0 scripts/ /app/scripts/
RUN find /app/scripts -type f -exec sed -i 's/\r$//' {} + \
 && find /app/scripts -type d -exec chmod 0755 {} + \
 && find /app/scripts -type f -exec chmod 0644 {} + \
 && find /app/scripts -type f -name '*.sh' -exec chmod 0755 {} + \
 && chown 0:0 /app/scripts

# Non-secret defaults; real values (SECURITY_TOKEN, BACKUP_*, ...) come from the env_file at runtime.
ENV HOST=0.0.0.0 \
    PORT=18080 \
    SCRIPTS_DIR=/app/scripts \
    SERVERS_DATA_DIR=/app/data \
    HOME=/home/${APP_USER} \
    TZ=Asia/Tehran

WORKDIR /app
USER ${APP_UID}:${APP_GID}

EXPOSE 18080
STOPSIGNAL SIGTERM

# /api/openapi.json is public (no token) and served by the app itself.
HEALTHCHECK --interval=15s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS -o /dev/null "http://127.0.0.1:${PORT}/api/openapi.json" || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/service-maintainer"]
