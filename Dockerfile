FROM rust:bookworm AS builder
WORKDIR /usr/src/chaz
COPY . .
RUN cargo build --release
RUN cargo install aichat

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y openssl libsqlite3-dev ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/chaz/target/release/chaz /usr/local/bin/chaz
COPY --from=builder /usr/local/cargo/bin/aichat /usr/local/bin/aichat
CMD ["chaz", "--config", "/config.yaml"]

