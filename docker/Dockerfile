# syntax=docker/dockerfile:1.7
FROM --platform=$BUILDPLATFORM node:24-alpine AS frontend-builder
WORKDIR /app
ARG USE_PREBUILT=false
COPY package.json pnpm-workspace.yaml ./
COPY packages/frontend ./packages/frontend
COPY client ./client
RUN if [ "$USE_PREBUILT" = "true" ] && [ -f client/index.html ]; then \
      echo "Using prebuilt frontend"; \
    else \
      npm install -g pnpm@11 && pnpm install --no-frozen-lockfile && pnpm build:frontend; \
    fi

FROM rust:1-alpine3.21 AS rust-builder
ARG TARGETARCH
WORKDIR /app
RUN apk add --no-cache build-base pkgconfig
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples/scriptcat-downloader.user.js ./examples/scriptcat-downloader.user.js
COPY --from=frontend-builder /app/client ./client
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=streamdock-target-${TARGETARCH},target=/app/target,sharing=locked \
    touch crates/server/src/assets.rs && \
    cargo build --release --locked -p streamdock-cli && \
    cp /app/target/release/streamdock /usr/local/bin/streamdock

FROM alpine:3.21
WORKDIR /app
ENV DS_PORT=6600 DS_SAVE_DIR=/app/downloads DS_CACHE_DIR=/app/cache RUST_LOG=info
RUN apk add --no-cache ffmpeg tini ca-certificates tzdata libgcc
COPY --from=rust-builder /usr/local/bin/streamdock /usr/local/bin/streamdock
VOLUME ["/app/downloads", "/app/cache"]
EXPOSE 6600
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD wget -qO- "http://127.0.0.1:${DS_PORT:-6600}/healthcheck" >/dev/null || exit 1
ENTRYPOINT ["/sbin/tini", "-g", "--"]
CMD ["streamdock", "server"]
