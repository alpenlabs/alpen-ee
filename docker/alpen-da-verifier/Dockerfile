ARG RUST_VERSION=1.92

FROM rust:${RUST_VERSION}-slim-trixie AS chef
WORKDIR /app

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_INCREMENTAL=0 \
    CARGO_TERM_COLOR=always

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential ca-certificates clang libclang-dev pkg-config libssl-dev protobuf-compiler libprotobuf-dev && \
    rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked

FROM chef AS planner
COPY rust-toolchain.toml ./
RUN rustup toolchain install
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY rust-toolchain.toml ./
RUN rustup toolchain install
COPY --from=planner /app/recipe.json recipe.json

RUN --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,sharing=private,target=/app/target \
    cargo chef cook --release --package alpen-da-verifier --recipe-path recipe.json

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,sharing=private,target=/app/target \
    cargo build --locked --release --package alpen-da-verifier --bin alpen-da-verifier && \
    install -m 0555 /app/target/release/alpen-da-verifier \
      /usr/local/bin/alpen-da-verifier

FROM debian:13-slim AS runtime

ENV APP_HOME=/app \
    HOME=/app \
    RUST_LOG=info

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir "${APP_HOME}" --uid 10001 \
      --shell /usr/sbin/nologin alpen && \
    mkdir -p /app/configs/generated /app/data && \
    chown -R alpen:alpen /app

WORKDIR "${APP_HOME}"

COPY --from=builder /usr/local/bin/alpen-da-verifier \
  /usr/local/bin/alpen-da-verifier
COPY docker/alpen-da-verifier/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0555 /usr/local/bin/alpen-da-verifier /usr/local/bin/entrypoint.sh

USER alpen

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
