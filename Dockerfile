FROM rust:1-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY migrations ./migrations
COPY sql ./sql
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl passwd \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home \
        --home-dir /var/lib/maven-haste maven-haste \
    && mkdir -p /var/lib/maven-haste /etc/maven-haste \
    && chown -R maven-haste:maven-haste /var/lib/maven-haste

COPY --from=build /src/target/release/maven-haste /usr/local/bin/maven-haste

USER maven-haste

# Cache data and the SQLite database live under this volume; put a
# maven-haste.toml next to it (for example in /etc/maven-haste).
VOLUME /var/lib/maven-haste
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/v1/health || exit 1

STOPSIGNAL SIGTERM
ENTRYPOINT ["maven-haste"]
CMD ["run", "-c", "/etc/maven-haste/maven-haste.toml"]
