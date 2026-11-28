# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder

WORKDIR /src

COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --locked -p mare-cli

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git bubblewrap tini \
    && rm -rf /var/lib/apt/lists/* \
    && git config --system --add safe.directory '*' \
    && groupadd -g 65532 nonroot \
    && useradd -u 65532 -g nonroot -m -s /bin/sh nonroot

COPY --from=builder /src/target/release/mare /usr/bin/mare
COPY LICENSE /usr/share/licenses/mare/LICENSE
COPY README.md /usr/share/doc/mare/README.md

USER nonroot:nonroot
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/bin/mare"]

LABEL org.opencontainers.image.title="mare"
LABEL org.opencontainers.image.description="Hyperextensible AI coding agent harness (Rust core, Lua 5.5 plugins)"
LABEL org.opencontainers.image.source="https://github.com/signalridge/mare"
LABEL org.opencontainers.image.vendor="SignalRidge"
LABEL org.opencontainers.image.licenses="Apache-2.0"
