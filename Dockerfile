# syntax=docker/dockerfile:1
FROM rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/
COPY crates/render/Cargo.toml crates/render/
COPY crates/plugins/Cargo.toml crates/plugins/
COPY crates/hub/Cargo.toml crates/hub/
RUN for c in core render plugins hub; do mkdir -p crates/$c/src && : > crates/$c/src/lib.rs; done \
    && echo 'fn main() {}' > crates/hub/src/main.rs \
    && cargo build --release --locked -p peephole-hub \
    && rm -rf crates/*/src target/release/.fingerprint/peephole-* target/release/peephole

COPY crates crates
COPY examples examples
RUN cargo build --release --locked -p peephole-hub \
    && install -D target/release/peephole /out/peephole

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /out/peephole /peephole
USER nonroot
EXPOSE 8080
ENV PEEPHOLE_LISTEN=0.0.0.0:8080
ENTRYPOINT ["/peephole"]
