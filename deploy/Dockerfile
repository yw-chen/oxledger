# SPDX-License-Identifier: GPL-2.0-or-later
# Local single-machine deployment (deploy/README.md). Build from the repo root:
#   docker compose -f deploy/compose.yaml build
# Two targets: `server` (oxledger serve) and `web` (Caddy: the SPA's static
# files, and /api, /mcp proxied to the server on the same origin).

# ---------------------------------------------------------------- SPA
FROM node:24-trixie-slim AS web-build
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
# $api resolves to ../api (vite.config.ts).
COPY api /src/api
COPY web /src/web
RUN npm run build

# ---------------------------------------------------------------- server
FROM rust:1.99-trixie AS server-build
# Half the CPUs of the build machine by default (cargo jobs).
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY migrations migrations
RUN cargo build --release --locked -p oxledger --features github,ai,google \
 && cp target/release/oxledger /usr/local/bin/oxledger

FROM debian:trixie-slim AS server
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data oxledger \
 && mkdir -p /data/index /files && chown -R oxledger /data /files
COPY --from=server-build /usr/local/bin/oxledger /usr/local/bin/oxledger
USER oxledger
WORKDIR /data
ENV OXLEDGER_BIND=0.0.0.0:8080 \
    OXLEDGER_INDEX_DIR=/data/index     OXLEDGER_FILES_DIR=/files
EXPOSE 8080
VOLUME ["/data", "/files"]
ENTRYPOINT ["oxledger"]
CMD ["serve"]

# ---------------------------------------------------------------- web (Caddy)
FROM caddy:2-alpine AS web
COPY deploy/Caddyfile /etc/caddy/Caddyfile
COPY --from=web-build /src/web/build /srv
