# Ludy's cloud on Rivet actors: the server, without the engine.
# Build from the repository root:  docker build -f providers/rivet/Dockerfile .
# (docker-compose.yml does this.)

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang libclang-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# The workspace's manifests have to be there for cargo to read the lock
# file; only ludy-rivet and what it depends on is compiled.
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
COPY providers/rivet providers/rivet
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ludy-rivet && cp target/release/ludy-rivet /usr/local/bin/ludy-rivet

FROM debian:bookworm-slim
# Certificates, for an S3 or an engine reached over HTTPS.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ludy-rivet /usr/local/bin/ludy-rivet
ENV LUDY_LISTEN=0.0.0.0:8080 \
    LUDY_BLOBS=/var/lib/ludy/blobs
VOLUME /var/lib/ludy/blobs
EXPOSE 8080
# Healthy means a drive answers, not only that the port is open: the engine
# is reachable and has given this node its actors. The command keeps asking
# for four seconds before it says no, so `docker compose up --wait` returns
# when the server becomes healthy and not at the next check.
HEALTHCHECK --interval=3s --timeout=6s --retries=5 --start-period=120s CMD ["ludy-rivet", "health"]
CMD ["ludy-rivet"]
