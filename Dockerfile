# syntax=docker/dockerfile:1

FROM rust:1.99.1-bookworm AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./

# Build the dependency graph once, so source-only changes reuse this layer.
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf '\n' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 tasklerq \
    && useradd --uid 10001 --gid tasklerq --no-create-home \
        --shell /usr/sbin/nologin tasklerq

COPY --from=builder /build/target/release/tasklerq /usr/local/bin/tasklerq

USER 10001:10001

EXPOSE 8080

ENV BIND_ADDR=0.0.0.0:8080

# Take the port from BIND_ADDR instead of duplicating 8080.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["sh", "-c", "exec /usr/local/bin/tasklerq healthcheck --url \"http://127.0.0.1:${BIND_ADDR##*:}/health/ready\""]

ENTRYPOINT ["/usr/local/bin/tasklerq"]
CMD ["serve"]
