# syntax=docker/dockerfile:1

# Build. rust:*-alpine images target musl natively, so the binary is fully static: the runtime
# stage needs no libc, no OpenSSL and no CA bundle (rustls compiles its trust roots in).
# buildx runs this stage once per requested platform under emulation, which keeps the recipe free
# of cross-compilation toolchains.
FROM rust:1.99-alpine AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked

# Stage the filesystem the final image gets.
#
# /tmp is world-writable and sticky: `scratch` has no filesystem of its own and the generated
# playlist cache lives there.
#
# /root/iptv is the directory every existing stack mounts (docker-compose mapped ./iptv to it and
# CATEGORY_FILTERS_PATH defaults to /root/iptv/category_filters.json). It is created here, owned by
# root, because the Go image ran as root and category filters are written back at runtime; running
# as an unprivileged user would break saving them on a host directory owned by root.
RUN mkdir -p /out/usr/local/bin /out/root/iptv \
    && mkdir -m 1777 /out/tmp \
    && cp target/release/iptv-proxy /out/usr/local/bin/iptv-proxy

# Run.
FROM scratch
COPY --from=builder /out /

# The Go image exposed 8080 and ran as root; both are kept so existing unRAID templates, port
# mappings and bind mounts behave exactly as before.
EXPOSE 8080
WORKDIR /root

LABEL org.opencontainers.image.title="iptv-proxy" \
      org.opencontainers.image.description="Reverse proxy for IPTV M3U subscriptions and Xtream Codes server APIs" \
      org.opencontainers.image.source="https://github.com/joesteffee/iptv-proxy" \
      org.opencontainers.image.licenses="GPL-3.0-only"

ENTRYPOINT ["/usr/local/bin/iptv-proxy"]
