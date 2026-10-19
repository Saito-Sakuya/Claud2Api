FROM rust:1-bookworm@sha256:e12566d5abc17a0da741628782cdb504b68b8961f5bfcf677936bf3e8b32de29 AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake perl pkg-config libclang-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY ui/ ui/
COPY data/ data/
ARG CARGO_BUILD_JOBS=4
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    touch src/lib.rs src/main.rs && cargo build --locked --release && install -m 0755 target/release/claud2api /claud2api
FROM gcr.io/distroless/cc-debian13:nonroot AS runtime
WORKDIR /app
COPY --from=build /claud2api /app/claud2api
COPY LICENSE THIRD_PARTY.md /app/
COPY LICENSES/ /app/LICENSES/
ENV PORT=8080 BIND_HOST=127.0.0.1 CONTROL_BIND=127.0.0.1:18082 CLAUD2API_DATA_DIR=/var/lib/claud2api
USER 65532:65532
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s CMD ["/app/claud2api", "--healthcheck"]
ENTRYPOINT ["/app/claud2api"]
