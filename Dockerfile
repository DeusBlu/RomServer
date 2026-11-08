# Static musl build; the runtime image only needs the binary.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev curl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY tools/fetch_htmx.sh ./tools/
COPY assets ./assets
RUN sh tools/fetch_htmx.sh
COPY src ./src
RUN cargo build --release --locked

FROM alpine:3
COPY --from=build /src/target/release/romserver /usr/local/bin/romserver
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh
ENV ROMSERVER_CONFIG=/config/settings/romserver.toml
# Owner RomServer gives everything it creates (Unraid's usual nobody:users), and a umask that
# keeps it writable over SMB. Override per install. The process itself stays root: Unraid's share
# settings on the boot stick are root-only.
ENV PUID=99 PGID=100 UMASK=000
EXPOSE 8080
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["serve"]
