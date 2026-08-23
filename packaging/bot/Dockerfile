# Meshcast Discord bot — container image.
#
#   docker build -f packaging/bot/Dockerfile -t meshcast-bot .
#   docker run -d --name meshcast-bot -e DISCORD_TOKEN=... -v meshcast-bot:/data ghcr.io/mattcree/meshcast-bot
#
# The bot keeps its state (iroh identity + links) in /data; mount a volume so
# users don't have to re-link after a container restart.

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake clang libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p meshcast-bot \
    && strip target/release/meshcast-bot

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /data --shell /usr/sbin/nologin meshcast
COPY --from=build /src/target/release/meshcast-bot /usr/local/bin/meshcast-bot
ENV MESHCAST_BOT_STATE_DIR=/data \
    RUST_LOG=warn,meshcast_bot=info,meshcast_signal=info
USER meshcast
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/meshcast-bot"]
