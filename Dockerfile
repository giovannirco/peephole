# syntax=docker/dockerfile:1
FROM rust:1.99-bookworm AS build
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

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/peephole /peephole
USER nonroot
EXPOSE 8080
ENV PEEPHOLE_LISTEN=0.0.0.0:8080
ENTRYPOINT ["/peephole"]
