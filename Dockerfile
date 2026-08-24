# syntax=docker/dockerfile:1

FROM rust:bookworm AS builder

WORKDIR /usr/src/copilot-api-proxy

# reqwest's TLS backend builds AWS-LC from source.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 copilot \
    && useradd --uid 10001 --gid copilot --create-home --home-dir /home/copilot copilot \
    && mkdir -p /home/copilot/.local/share/copilot-api-proxy \
        /home/copilot/.cache/Microsoft/DeveloperTools \
    && chown -R copilot:copilot /home/copilot

COPY --from=builder /usr/src/copilot-api-proxy/target/release/copilot-api-proxy \
    /usr/local/bin/copilot-api-proxy

ENV HOME=/home/copilot
WORKDIR /home/copilot
USER copilot:copilot

EXPOSE 9876
STOPSIGNAL SIGINT

ENTRYPOINT ["copilot-api-proxy"]
CMD ["server", "--host", "0.0.0.0", "--port", "9876"]
