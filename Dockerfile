FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef

WORKDIR /app

FROM chef AS planner

COPY Cargo.toml ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

FROM debian:trixie-slim

WORKDIR /app

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/rinha_leaderboard /usr/local/bin/rinha_leaderboard

EXPOSE 3000

CMD ["rinha_leaderboard"]
