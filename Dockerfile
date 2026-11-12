# syntax=docker/dockerfile:1
# The whole build is offline: every crate comes from ./vendor and the runtime image needs no package install.
FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY .cargo-docker/config.toml /usr/local/cargo/config.toml
COPY vendor vendor
# Dependency layer: only re-runs when Cargo.toml / Cargo.lock / vendor change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked --offline \
    && rm -rf src target/release/shopcore target/release/deps/shopcore*
COPY migrations migrations
COPY seed seed
COPY src src
RUN cargo build --release --locked --offline

# python:3.13-slim already ships CA certificates and Python (only used by the manage.py launcher).
FROM python:3.13-slim
ENV TZ=Asia/Jakarta \
    PYTHONDONTWRITEBYTECODE=1
RUN useradd --system --uid 10001 --home-dir /srv/shopcore shop \
    && mkdir -p /srv/shopcore /tmp/exports \
    && chown shop /srv/shopcore /tmp/exports
COPY --from=build /src/target/release/shopcore /usr/local/bin/shopcore
COPY manage /usr/local/bin/manage
COPY manage.py /srv/shopcore/manage.py
WORKDIR /srv/shopcore
USER shop
EXPOSE 8000
HEALTHCHECK --interval=5s --timeout=3s --start-period=20s --retries=10 CMD ["shopcore", "healthcheck"]
# Starting the service also creates the tables and seeds the admin account, coupons and products.
CMD ["shopcore", "serve"]
