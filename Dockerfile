# ---- build ----
# Pinned: the floating "rust:1" tag shipped a compiler (1.100.0) that crashes on `syn`.
# Change with: docker build --build-arg RUST_VERSION=1.xx .
ARG RUST_VERSION=1.90
FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /app
COPY Cargo.toml ./
# Pre-build dependencies for better layer caching
RUN mkdir src static && echo "fn main(){}" > src/main.rs && echo "" > static/index.html \
 && cargo build --release \
 && rm -rf src static
COPY src ./src
COPY static ./static
RUN touch src/main.rs && cargo build --release

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data wuc \
 && mkdir /data && chown wuc /data
COPY --from=build /app/target/release/winupdate-cache /usr/local/bin/winupdate-cache
USER wuc
ENV DATA_DIR=/data
EXPOSE 8080 8081
VOLUME /data
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD ["/usr/local/bin/winupdate-cache", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/winupdate-cache"]
