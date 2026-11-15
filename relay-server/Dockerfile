# syntax=docker/dockerfile:1
# Build from the repository root:
#   docker build -f relay-server/Dockerfile -t aether-relay .
#
# Stage 1: fully static musl binary.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# Only the relay (and its deps) is compiled; the rest of the workspace is just parsed.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p aether-relay \
 && cp target/release/aether-relay /aether-relay \
 && strip /aether-relay

# Stage 2: nothing but the binary (≈ 3–4 MB image).
FROM scratch
COPY --from=build /aether-relay /aether-relay
USER 65534:65534
# 21116/udp: rendezvous + STUN. Relay legs: publish the range given to --relay-ports.
EXPOSE 21116/udp
EXPOSE 30000-30999/udp
ENTRYPOINT ["/aether-relay"]
CMD ["--bind", "0.0.0.0:21116", "--relay-ports", "30000-30999", "--workers", "2"]
