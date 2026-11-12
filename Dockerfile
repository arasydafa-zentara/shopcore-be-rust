# syntax=docker/dockerfile:1
FROM rust:1.99-bookworm AS build
WORKDIR /src
# Dependency layer: only re-runs when Cargo.toml / Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/shopcore target/release/deps/shopcore*
COPY migrations migrations
COPY seed seed
COPY src src
RUN cargo build --release --locked

FROM debian:bookworm-slim
ENV TZ=Asia/Jakarta
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /srv/shopcore shop \
    && mkdir -p /srv/shopcore /tmp/exports \
    && chown shop /srv/shopcore /tmp/exports
COPY --from=build /src/target/release/shopcore /usr/local/bin/shopcore
COPY manage /usr/local/bin/manage
COPY manage /srv/shopcore/manage.py
WORKDIR /srv/shopcore
USER shop
EXPOSE 8000
HEALTHCHECK --interval=5s --timeout=3s --start-period=20s --retries=10 CMD ["shopcore", "healthcheck"]
# Starting the service also creates the tables and seeds the admin account, coupons and products.
CMD ["shopcore", "serve"]
