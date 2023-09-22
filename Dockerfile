FROM rust:1.72-slim-bookworm as builder
WORKDIR /usr/src/myapp
COPY . .
RUN cargo install --path .

FROM debian:bookworm-slim
RUN apt-get update & apt-get install -y extra-runtime-dependencies & rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/pgdproxy /usr/local/bin/pgdproxy
CMD ["pgdproxy"]