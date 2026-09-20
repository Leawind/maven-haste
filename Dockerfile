FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/maven-haste /usr/local/bin/maven-haste
# Cache data and the SQLite database live under this volume; put a
# maven-haste.toml next to it (for example in /etc/maven-haste).
VOLUME /var/lib/maven-haste
EXPOSE 8080
ENTRYPOINT ["maven-haste"]
CMD ["run", "-c", "/etc/maven-haste/maven-haste.toml"]
