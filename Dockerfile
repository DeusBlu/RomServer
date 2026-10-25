# Static musl build; the runtime image only needs the binary.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM alpine:3
COPY --from=build /src/target/release/romserver /usr/local/bin/romserver
ENV ROMSERVER_CONFIG=/config/settings/romserver.toml
ENTRYPOINT ["romserver"]
