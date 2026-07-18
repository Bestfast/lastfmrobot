# syntax=docker/dockerfile:1
#
# Build a multi-arch image (linux/amd64 + linux/arm64):
#   docker buildx build --platform linux/amd64,linux/arm64 -t lastfmrobot --push .
#
# src/config.rs (secrets, gitignored) must exist locally before building - see
# src/config.rs.example for the format. It is compiled into the binary as Rust
# consts, so it's only needed at build time.
#
# The CJK font (config::FONT_FILE_PATH, ~170MB) and the sqlite database are
# NOT baked into the image to keep it small and rebuilds fast. Bind-mount them
# into the container at runtime instead:
#   docker run \
#     -v ./NotoSerifCJK.ttc:/app/NotoSerifCJK.ttc:ro \
#     -v ./users.sqlite:/app/users.sqlite \
#     lastfmrobot

FROM rust:alpine AS builder

RUN apk add --no-cache musl-dev gcc pkgconf sqlite-dev openssl-dev

# musl targets default to fully static linking (crt-static), but Alpine's
# sqlite-dev/openssl-dev only ship .so files, not .a static archives. Link
# dynamically instead - the runtime stage already installs sqlite-libs/libssl3.
ENV RUSTFLAGS="-C target-feature=-crt-static"

WORKDIR /app

# Cache dependency compilation in its own layer, separate from source changes.
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

COPY src ./src
RUN apk add --no-cache ca-certificates libgcc sqlite-libs libssl3
RUN touch src/main.rs && cargo build --release --locked

FROM alpine:latest

RUN apk add --no-cache ca-certificates libgcc sqlite-libs libssl3

WORKDIR /app

COPY --from=builder /app/target/release/lastfmrobot ./lastfmrobot
COPY everynoise_genres.txt ./everynoise_genres.txt

ENTRYPOINT ["./lastfmrobot"]
