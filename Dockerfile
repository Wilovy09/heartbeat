# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# UI strings, templates, static files and vendored libs are embedded in the binary.
COPY locales ./locales
COPY templates ./templates
COPY static ./static
COPY libs ./libs
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /app heartbeat
WORKDIR /app
COPY --from=build /src/target/release/heartbeat /usr/local/bin/heartbeat
RUN mkdir -p data && chown heartbeat data
USER heartbeat
ENV HOST=0.0.0.0 PORT=8090
EXPOSE 8090
VOLUME ["/app/data"]
HEALTHCHECK --interval=30s --timeout=5s CMD curl -fsS http://localhost:8090/healthz || exit 1
ENTRYPOINT ["heartbeat"]
