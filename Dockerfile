# syntax=docker/dockerfile:1
#
# forklift-server container image.
#
#   docker build -t forklift-server .
#   docker run -p 9418:9418 -v forklift-data:/data forklift-server \
#       serve --warehouses /data --addr 0.0.0.0:9418 --token <admin-secret>
#
# Serves every prepared warehouse under /data (multi-warehouse mode). Create warehouses via
# the admin API and set a token — see docs/SERVER.md. There is deliberately no server
# self-update: to upgrade, pull a new image and restart (docs/SERVER.md § Updating).
#
# The default CMD below carries no token and refuses to start on its own (see docs/SERVER.md,
# "Authentication"): forklift-server requires an explicit token/tokens/authentication-hook
# config, or an explicit --open opt-out, and never infers "open" from an absent one. This is
# deliberate — a plain `docker run` with no override must fail loud, not silently serve the
# world from 0.0.0.0. Override the full command (as above) with --token, or with --open for a
# throwaway/local container, to actually start it.

# ── build ─────────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p forklift-server

# ── runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# rustls uses the system trust store for outbound TLS (provider hooks); no OpenSSL needed.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run unprivileged; /data holds the served warehouses.
RUN useradd --system --uid 10001 --home-dir /data forklift \
    && mkdir -p /data \
    && chown forklift:forklift /data

COPY --from=build /src/target/release/forklift-server /usr/local/bin/forklift-server

USER forklift
WORKDIR /data
VOLUME /data
EXPOSE 9418

# Multi-warehouse mode by default. Override the command for a single-warehouse root
# (`serve --root /data/wh`), to set a token (`--token …`), or to point at a config file.
ENTRYPOINT ["forklift-server"]
CMD ["serve", "--warehouses", "/data", "--addr", "0.0.0.0:9418"]
