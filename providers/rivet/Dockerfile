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
COPY providers providers
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
CMD ["ludy-rivet"]
