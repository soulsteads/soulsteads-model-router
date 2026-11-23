FROM docker.io/library/rust:1.100.0-trixie@sha256:223cc32ddca8f3ee90c436dbc94b6b5f052ea7f9653c5201e9612f08ad7d21b8 AS build
WORKDIR /src
ENV CARGO_BUILD_JOBS=2
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
RUN cargo build --release --locked --bin slotline

FROM docker.io/library/debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
LABEL org.opencontainers.image.source="https://github.com/soulsteads/soulsteads-model-router"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 app \
    && useradd --uid 10001 --gid 10001 --home-dir /data/home --no-create-home app \
    && install -d -m 0700 -o 10001 -g 10001 /data /data/home
COPY LICENSE NOTICE third_party/LiteLLM-LICENSE.txt /usr/share/doc/soulsteads-model-router/
COPY --from=build /src/target/release/slotline /usr/local/bin/slotline
ENV HOME=/data/home
USER 10001:10001
WORKDIR /data
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/slotline"]
CMD ["/data/config.yaml"]
